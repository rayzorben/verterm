//! The vertical tab rail: bucket → group → tab rows drawn as cards with the painter. Rows have
//! per-kind heights so a tab with a subtitle gets two real text baselines. Pure presentation;
//! the App builds the rows and applies the output.
//!
//! The tree is *nested boxes*, not an indented list: a bucket is a bounded, rounded panel with
//! its category header capping the top, each group inside it is a band tinted with its own hue
//! and tied to its tabs by a coloured spine, and the tab cards sit inside that band. The point
//! is to make "which session do I want" a matter of aiming at a coloured region rather than
//! reading every row. Because a panel's height is only known after its rows are laid out, each
//! one reserves a shape slot before its first row (so it paints *under* them) and fills it in
//! when the span closes — see [`Panel`].

use std::time::Duration;

use egui::{
    Color32, CornerRadius, Rect, Sense, Shape, Stroke, StrokeKind, epaint::RectShape,
    layers::ShapeIdx, pos2, vec2,
};

use super::chrome::{self, Chip, Icon, Typography};
use super::theme::{GROUP_TINTS, UiColors, tint};
use crate::session::TabId;

/// Card inset from the rail edges.
const PAD_X: f32 = 8.0;
/// Vertical gap between cards.
const GAP_Y: f32 = 2.0;
/// Width of the keyboard-index gutter inside a tab card.
const GUTTER: f32 = 16.0;
/// Side of the kind mark drawn after the index gutter.
const ICON: f32 = 12.0;
/// Width of the CPU gauge on the metric line, and the tallest it is allowed to be. The height
/// is clamped to the small font's line box so the gauge never outgrows the row it sits on.
const GAUGE_W: f32 = 40.0;
const GAUGE_H: f32 = 13.0;
/// The narrowest git chip still worth drawing, as a multiple of the micro font size. Below
/// roughly five characters it is noise rather than information, and the row's tooltip carries
/// the branch in full either way.
const GIT_MIN_EMS: f32 = 3.5;
/// Width the subtitle text keeps for itself whatever else wants the row.
const SUB_FLOOR: f32 = 70.0;
/// Padding above and below the text block inside a tab card.
const CARD_PAD_Y: f32 = 6.0;
/// Space between a tab's title and its subtitle.
const LINE_GAP: f32 = 2.0;
/// Inset of a group band inside its bucket panel.
const GROUP_INSET: f32 = 4.0;
/// Inset of a tab card inside its group band. The left side is wider because the group's
/// coloured spine runs down it.
const TAB_INSET_L: f32 = 6.0;
const TAB_INSET_R: f32 = 3.0;
/// Width of that spine.
const SPINE_W: f32 = 2.5;
/// Corner radius of a bucket panel and of a group band.
const R_BUCKET: u8 = 10;
const R_GROUP: u8 = 7;

// The boxes have to actually nest: a bucket panel wide enough for a group band inside it, a
// band wide enough for a tab card *plus* the spine that ties the two together, and an outer
// radius larger than the inner one so the smaller box reads as sitting in the bigger one.
// Tuning any of these by eye is easy; tuning them until the nesting collapses back into a flat
// list is just as easy, which is why they are checked at compile time rather than by eye.
const _: () = assert!(GROUP_INSET > 0.0);
const _: () = assert!(TAB_INSET_L > SPINE_W, "the spine must fit beside the card");
const _: () = assert!(TAB_INSET_R > 0.0);
const _: () = assert!(R_BUCKET > R_GROUP, "the outer box reads as the wider one");

#[derive(Clone, Debug)]
pub enum Indicator {
    Idle,
    Running { since: Duration },
    Success,
    Failure(i32),
    Exited,
}

#[derive(Clone, Debug)]
pub struct Badge {
    pub text: String,
    pub color: Color32,
}

impl Badge {
    pub fn new(text: impl Into<String>, color: Color32) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TabRow {
    pub id: TabId,
    /// 1-based position in the flattened tree (Alt+N target when ≤ 9).
    pub index: usize,
    pub title: String,
    /// What kind of session this is, drawn as a mark so the row is recognisable without
    /// reading it. Identity only — the dot beside it still carries lifecycle state.
    pub icon: Icon,
    /// Colour of that mark: the kind's own colour, not the session's state.
    pub icon_color: Color32,
    /// Second line, left segment: `name · memory`.
    pub subtitle: Option<String>,
    /// Foreground CPU: the fraction of the whole machine that fills the gauge, and the reading
    /// printed on top of it. `None` below 1%, where a gauge would be noise.
    pub load: Option<(f32, String)>,
    pub indicator: Indicator,
    /// Identity chips other than git — `ssh <host>`, `box <name>`, `root`.
    pub badges: Vec<Badge>,
    /// The git branch, kept apart from the rest because it is pinned to the right edge of the
    /// metric line.
    pub git: Option<Badge>,
    pub elevated: bool,
    pub active: bool,
    pub bell: bool,
}

#[derive(Clone, Debug)]
pub enum Row {
    Bucket {
        key: String,
        label: &'static str,
        icon: Icon,
        collapsed: bool,
        count: usize,
        elevated: bool,
        /// The category's own hue — it washes the whole panel, so the App picks it from the
        /// bucket rather than the rail re-deriving it from the key string.
        tint: Color32,
    },
    Group {
        key: String,
        name: String,
        icon: Icon,
        collapsed: bool,
        count: usize,
        elevated: bool,
    },
    Tab(TabRow),
}

#[derive(Default)]
pub struct RailOutput {
    pub activate: Option<TabId>,
    pub toggle_key: Option<String>,
    pub close: Option<TabId>,
    /// Double-click on empty rail space, or "New tab" from a context menu.
    pub new_tab: bool,
    pub new_scratchpad: bool,
    /// Move the tab to the next/previous group (`true` = next).
    pub move_group: Option<(TabId, bool)>,
    /// Copy this tab's working directory to the clipboard.
    pub copy_cwd: Option<TabId>,
    pub collapse_all: bool,
    pub expand_all: bool,
    /// The find box was clicked, so it should take the keyboard.
    pub focus_find: bool,
    /// The query text changed this frame.
    pub find_changed: bool,
}

/// Dot colour for a tab's lifecycle state, and whether it should pulse with a halo.
pub fn indicator_color(ind: &Indicator, colors: &UiColors, time: f64) -> (Color32, bool) {
    match ind {
        Indicator::Idle => (colors.faint, false),
        Indicator::Running { since } => {
            if *since >= Duration::from_secs(3) {
                let pulse = 0.55 + 0.45 * ((time * 3.0).sin() as f32);
                (colors.amber.gamma_multiply(pulse.clamp(0.3, 1.0)), true)
            } else {
                (colors.amber, false)
            }
        }
        Indicator::Success => (colors.green, false),
        Indicator::Failure(_) => (colors.red, false),
        Indicator::Exited => (colors.faint, false),
    }
}

/// A container whose height is only known once the rows inside it have been laid out.
///
/// egui lays rows out top-down, so a panel behind a span of them cannot be painted when the
/// span opens — but it also must not be painted *after*, or it would cover its own contents.
/// The way out is epaint's two-step: reserve a slot in the shape list up front (`Painter::add`
/// of a `Shape::Noop`, which fixes the paint order) and fill it in when the span closes
/// (`Painter::set`). Both calls use the same painter so the reserved slot keeps the scroll
/// area's clip rect.
struct Panel {
    slot: ShapeIdx,
    left: f32,
    right: f32,
    top: f32,
    /// Bottom edge of the last row seen inside this panel.
    bottom: f32,
    /// Signed adjustments applied to those two edges when the panel closes. Rows tile without
    /// gaps, so a panel that used them raw would touch its neighbour above and below and the
    /// stack would read as one box again; this is where the air between boxes comes from.
    pad: (f32, f32),
    style: PanelStyle,
}

/// How a [`Panel`] is painted. A struct rather than four more parameters because the two call
/// sites differ only in these, and they read better named at the call.
#[derive(Clone, Copy)]
struct PanelStyle {
    fill: Color32,
    stroke: Color32,
    radius: u8,
    /// Left spine, drawn for groups: it is what ties a group's tab cards to its header.
    spine: Option<Color32>,
}

impl Panel {
    fn open(
        p: &egui::Painter,
        rect: Rect,
        (left, right): (f32, f32),
        pad: (f32, f32),
        style: PanelStyle,
    ) -> Self {
        Self {
            slot: p.add(Shape::Noop),
            left,
            right,
            top: rect.top(),
            bottom: rect.bottom(),
            pad,
            style,
        }
    }

    /// Top edge as it will be painted — the header cap has to line up with it exactly.
    fn painted_top(&self) -> f32 {
        self.top + self.pad.0
    }

    fn close(self, p: &egui::Painter) {
        let top = self.painted_top();
        let rect = Rect::from_min_max(
            pos2(self.left, top),
            pos2(self.right, (self.bottom + self.pad.1).max(top)),
        );
        let radius = CornerRadius::same(self.style.radius);
        let mut shapes: Vec<Shape> = vec![
            RectShape::new(
                rect,
                radius,
                self.style.fill,
                Stroke::new(1.0, self.style.stroke),
                StrokeKind::Inside,
            )
            .into(),
        ];
        if let Some(color) = self.style.spine {
            // Inset by the stroke so the spine reads as a bar *inside* the band rather than a
            // thicker left border.
            let spine = Rect::from_min_max(
                pos2(rect.left() + 1.0, rect.top() + 3.0),
                pos2(rect.left() + 1.0 + SPINE_W, rect.bottom() - 3.0),
            );
            shapes.push(RectShape::filled(spine, CornerRadius::same(1), color).into());
        }
        p.set(self.slot, Shape::Vec(shapes));
    }
}

/// FNV-1a over the group key. A hand-rolled hash rather than `DefaultHasher` because the value
/// has to mean the same thing in the next process: a group that was blue this morning being
/// green after a restart would defeat the point of colouring it at all.
fn key_hash(key: &str) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h as usize
}

/// Hue index for each group, in the order the groups appear.
///
/// Two properties, in this priority order, because they conflict: a group should keep the same
/// hue across restarts (so the colour becomes part of how you recognise it), *and* two groups
/// that touch must never share one (so the boxes actually separate). The hash gives the first;
/// nudging a collision with its predecessor to the next hue gives the second, and only
/// perturbs the group that would have been ambiguous.
fn group_tints(keys: &[&str], n: usize) -> Vec<usize> {
    if n == 0 {
        return vec![0; keys.len()];
    }
    let mut out: Vec<usize> = keys.iter().map(|k| key_hash(k) % n).collect();
    if n > 1 {
        for i in 1..out.len() {
            if out[i] == out[i - 1] {
                out[i] = (out[i] + 1) % n;
            }
        }
    }
    out
}

/// Vertical geometry of a tab card, derived from the real line boxes of the two fonts: the row
/// is sized to fit its lines rather than the lines being squeezed into a fixed row. Offsets are
/// measured from the top of the card.
#[derive(Clone, Copy, Debug)]
struct TabGeometry {
    /// Height of the whole row, including the gaps that separate one card from the next.
    row_h: f32,
    title_cy: f32,
    sub_cy: Option<f32>,
}

/// Height of the line box a font of `size` occupies.
fn line_box(size: f32) -> f32 {
    (size * 1.4).round()
}

impl TabGeometry {
    fn new(ui_size: f32, small_size: f32, has_subtitle: bool) -> Self {
        let title_box = line_box(ui_size);
        let sub_box = line_box(small_size);
        let inner = if has_subtitle {
            title_box + LINE_GAP + sub_box
        } else {
            title_box
        };
        Self {
            row_h: inner + CARD_PAD_Y * 2.0 + GAP_Y * 2.0,
            title_cy: CARD_PAD_Y + title_box / 2.0,
            sub_cy: has_subtitle.then(|| CARD_PAD_Y + title_box + LINE_GAP + sub_box / 2.0),
        }
    }
}

fn row_height(row: &Row, ty: &Typography) -> f32 {
    match row {
        Row::Bucket { .. } => (ty.ui.size * 2.1).round(),
        Row::Group { .. } => (ty.ui.size * 2.0).round(),
        Row::Tab(t) => TabGeometry::new(ty.ui.size, ty.small.size, t.subtitle.is_some()).row_h,
    }
}

/// The find box that sits above the tree.
pub struct FindBox<'a> {
    pub query: &'a mut String,
    /// Whether the box currently owns the keyboard (`Mode::FindSession`).
    pub focused: bool,
    /// `Some(n)` while a query is active: how many result rows are being shown.
    pub results: Option<usize>,
    /// `[general].show_icon` — draw the verterm mark above the box.
    pub show_icon: bool,
}

pub fn show(
    ui: &mut egui::Ui,
    rows: &[Row],
    colors: &UiColors,
    ty: &Typography,
    time: f64,
    find: FindBox<'_>,
) -> RailOutput {
    let mut out = RailOutput::default();
    let base = ty.ui.size;

    if find.show_icon {
        ui.add_space(7.0);
        let h = (base * 1.8).round();
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
        let p = ui.painter();
        // The mark carries a rail divider and three tabs inside it; below ~22 px those merge,
        // so it is floored rather than tracking the font all the way down.
        let mark = h.clamp(20.0, 24.0);
        chrome::brand(
            p,
            pos2(rect.left() + PAD_X + 2.0 + mark / 2.0, rect.center().y),
            mark,
            colors.fg.gamma_multiply(0.85),
            colors.vermilion,
        );
        chrome::line(
            p,
            rect.left() + PAD_X + mark + 10.0,
            rect.center().y,
            "verterm",
            &ty.strong,
            colors.fg.gamma_multiply(0.72),
            (rect.width() - mark - 28.0).max(0.0),
        );
        ui.add_space(2.0);
    } else {
        ui.add_space(6.0);
    }
    ui.horizontal(|ui| {
        ui.add_space(PAD_X);
        let te = egui::TextEdit::singleline(find.query)
            .font(ty.small.clone())
            .desired_width(ui.available_width() - PAD_X)
            .hint_text(
                egui::RichText::new("find session or text…")
                    .font(ty.small.clone())
                    .color(colors.faint),
            )
            .text_color(colors.fg)
            .frame(chrome::well_frame(colors))
            .show(ui);
        if te.response.clicked() {
            out.focus_find = true;
        }
        // egui's focus is independent of the app's `Mode`, and if the two disagree the box
        // keeps swallowing keystrokes that the terminal is *also* processing — the same
        // characters land in both. Focus is therefore driven both ways, every frame.
        if find.focused {
            te.response.request_focus();
        } else if te.response.has_focus() {
            te.response.surrender_focus();
        }
        if te.response.changed() {
            out.find_changed = true;
        }
    });
    if let Some(n) = find.results {
        ui.add_space(2.0);
        let (rect, _) = ui.allocate_exact_size(
            vec2(ui.available_width(), line_box(ty.micro.size)),
            Sense::hover(),
        );
        let label = match n {
            0 => "no match".to_string(),
            1 => "1 result".to_string(),
            n => format!("{n} results"),
        };
        chrome::section_label(
            ui.painter(),
            pos2(rect.left() + PAD_X + 2.0, rect.center().y),
            &label,
            &ty.micro,
            colors.faint,
        );
    }

    // Hues are decided for the whole tree up front so a group's colour depends only on the
    // groups above it, not on which of them happen to be scrolled into view.
    let tints = {
        let keys: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Group { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        group_tints(&keys, GROUP_TINTS)
    };
    let mut next_tint = 0usize;

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(4.0);
            // The two open panels. A bucket closes when the next bucket starts or the list
            // ends; a group closes on either of those *or* the next group.
            let mut bucket: Option<Panel> = None;
            let mut group: Option<(Panel, Color32)> = None;

            for (i, row) in rows.iter().enumerate() {
                // Buckets open a new section; give them air unless they lead the list.
                if i > 0 && matches!(row, Row::Bucket { .. }) {
                    ui.add_space(base * 0.6);
                }
                let (rect, resp) = ui.allocate_exact_size(
                    vec2(ui.available_width(), row_height(row, ty)),
                    Sense::click(),
                );
                let bucket_span = (rect.left() + PAD_X, rect.right() - PAD_X);
                let group_span = (bucket_span.0 + GROUP_INSET, bucket_span.1 - GROUP_INSET);

                // Close whatever this row ends before opening anything, so panels nest rather
                // than overlap, and so a panel's slot is always reserved before its own rows.
                match row {
                    Row::Bucket { .. } => {
                        if let Some((g, _)) = group.take() {
                            g.close(ui.painter());
                        }
                        if let Some(b) = bucket.take() {
                            b.close(ui.painter());
                        }
                    }
                    Row::Group { .. } => {
                        if let Some((g, _)) = group.take() {
                            g.close(ui.painter());
                        }
                    }
                    Row::Tab(_) => {}
                }
                if let Some(b) = bucket.as_mut() {
                    b.bottom = rect.bottom();
                }
                if let Some((g, _)) = group.as_mut() {
                    g.bottom = rect.bottom();
                }

                let painter = ui.painter().with_clip_rect(rect);
                match row {
                    Row::Bucket {
                        key,
                        label,
                        icon,
                        collapsed,
                        count,
                        elevated,
                        tint: hue,
                    } => {
                        let hue = if *elevated { colors.vermilion } else { *hue };
                        // The panel is reserved on the *unclipped* painter: it will grow past
                        // this row, so clipping it to the header would cut it off.
                        bucket = Some(Panel::open(
                            ui.painter(),
                            rect,
                            bucket_span,
                            (0.0, 3.0),
                            PanelStyle {
                                fill: tint(hue, 13),
                                stroke: tint(hue, 78),
                                radius: R_BUCKET,
                                spine: None,
                            },
                        ));
                        let card = Rect::from_min_max(
                            pos2(bucket_span.0, rect.top()),
                            pos2(bucket_span.1, rect.bottom()),
                        );
                        // The category header caps the panel: a stronger wash of the same hue
                        // with only its top corners rounded, so it reads as a title bar on the
                        // box rather than as one more row inside it. Inset by the panel's own
                        // stroke so it does not paint over the outline.
                        let cap = Rect::from_min_max(
                            pos2(card.left() + 1.0, card.top() + 1.0),
                            pos2(card.right() - 1.0, card.bottom()),
                        );
                        let cap_r = if *collapsed {
                            CornerRadius::same(R_BUCKET - 1)
                        } else {
                            CornerRadius {
                                nw: R_BUCKET - 1,
                                ne: R_BUCKET - 1,
                                sw: 0,
                                se: 0,
                            }
                        };
                        painter.rect_filled(cap, cap_r, tint(hue, 30));
                        if !*collapsed {
                            painter.line_segment(
                                [
                                    pos2(cap.left(), cap.bottom() - 0.5),
                                    pos2(cap.right(), cap.bottom() - 0.5),
                                ],
                                Stroke::new(1.0, tint(hue, 58)),
                            );
                        }
                        if resp.hovered() {
                            painter.rect_filled(cap, cap_r, tint(hue, 18));
                        }
                        chrome::chevron(
                            &painter,
                            pos2(card.left() + 11.0, card.center().y),
                            3.5,
                            !*collapsed,
                            hue,
                        );
                        chrome::icon(
                            &painter,
                            *icon,
                            pos2(card.left() + 23.0, card.center().y),
                            ICON - 1.0,
                            hue,
                        );
                        chrome::section_label(
                            &painter,
                            pos2(card.left() + 34.0, card.center().y),
                            label,
                            &ty.micro,
                            hue,
                        );
                        let chip = Chip::plain(&painter, &count.to_string(), &ty.micro, hue);
                        chip.paint(&painter, card.right() - 9.0 - chip.width(), card.center().y);
                        if resp.clicked() {
                            out.toggle_key = Some(key.clone());
                        }
                    }
                    Row::Group {
                        key,
                        name,
                        icon,
                        collapsed,
                        count,
                        elevated,
                    } => {
                        let hue = if *elevated {
                            colors.vermilion
                        } else {
                            let t = tints.get(next_tint).copied().unwrap_or(0);
                            colors.group[t.min(GROUP_TINTS - 1)]
                        };
                        next_tint += 1;
                        group = Some((
                            Panel::open(
                                ui.painter(),
                                rect,
                                group_span,
                                (2.0, -1.0),
                                PanelStyle {
                                    fill: tint(hue, 18),
                                    stroke: tint(hue, 52),
                                    radius: R_GROUP,
                                    spine: Some(tint(hue, 190)),
                                },
                            ),
                            hue,
                        ));
                        let card = Rect::from_min_max(
                            pos2(group_span.0, rect.top() + 2.0),
                            pos2(group_span.1, rect.bottom()),
                        );
                        if resp.hovered() {
                            painter.rect_filled(card, CornerRadius::same(R_GROUP), tint(hue, 22));
                        }
                        chrome::chevron(
                            &painter,
                            pos2(card.left() + 12.0, card.center().y),
                            3.5,
                            !*collapsed,
                            hue,
                        );
                        let mut right = card.right() - 6.0;
                        if *collapsed {
                            let chip =
                                Chip::plain(&painter, &count.to_string(), &ty.micro, colors.faint);
                            right -= chip.width();
                            chip.paint(&painter, right, card.center().y);
                            right -= 6.0;
                        }
                        chrome::icon(
                            &painter,
                            *icon,
                            pos2(card.left() + 24.0, card.center().y),
                            ICON,
                            hue,
                        );
                        let x = card.left() + 34.0;
                        // A group is a header for the tabs under it, so it reads at the same
                        // size as their titles; `ty.small` made the heading quieter than its
                        // own children and inverted the hierarchy.
                        chrome::line(
                            &painter,
                            x,
                            card.center().y,
                            name,
                            &ty.ui,
                            hue,
                            (right - x).max(0.0),
                        );
                        if resp.clicked() {
                            out.toggle_key = Some(key.clone());
                        }
                    }
                    Row::Tab(tab) => {
                        // A tab sits inside its group band when it has one, and directly in
                        // the bucket panel when it does not (the find view lists tabs under a
                        // heading with no group).
                        let (l, r) = match &group {
                            Some(_) => (group_span.0 + TAB_INSET_L, group_span.1 - TAB_INSET_R),
                            None => (bucket_span.0 + TAB_INSET_R, bucket_span.1 - TAB_INSET_R),
                        };
                        let card = Rect::from_min_max(
                            pos2(l, rect.top() + GAP_Y),
                            pos2(r, rect.bottom() - GAP_Y),
                        );
                        draw_tab(&painter, card, tab, colors, ty, time, resp.hovered());
                        if resp.clicked() {
                            out.activate = Some(tab.id);
                        }
                        // Middle-click closes, the way browsers and other terminals do.
                        if resp.middle_clicked() {
                            out.close = Some(tab.id);
                        }
                        // A bare right-click used to close the tab outright — one stray click
                        // killed a session. It opens a menu instead; Close is now deliberate.
                        let id = tab.id;
                        // The card can only ever show a fitted branch, a packed subset of the
                        // chips and an elided path; hovering is the way back to what they
                        // actually are.
                        let resp = resp.on_hover_text(tab_tooltip(tab));
                        resp.context_menu(|ui| {
                            if ui.button("Activate").clicked() {
                                out.activate = Some(id);
                                ui.close();
                            }
                            if ui.button("Copy working directory").clicked() {
                                out.copy_cwd = Some(id);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Move to next group").clicked() {
                                out.move_group = Some((id, true));
                                ui.close();
                            }
                            if ui.button("Move to previous group").clicked() {
                                out.move_group = Some((id, false));
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("New tab").clicked() {
                                out.new_tab = true;
                                ui.close();
                            }
                            if ui.button("Close tab").clicked() {
                                out.close = Some(id);
                                ui.close();
                            }
                        });
                    }
                }
            }
            if let Some((g, _)) = group.take() {
                g.close(ui.painter());
            }
            if let Some(b) = bucket.take() {
                b.close(ui.painter());
            }
            // Empty space below the last row is still part of the tab bar: a double-click
            // there opens a tab (the usual tab-bar gesture) and a right-click offers the
            // rail-wide actions. Claim at least a comfortable target even when the list is
            // long, so the gesture is always reachable at the bottom of the rail.
            ui.add_space(8.0);
            let empty = ui.available_size_before_wrap();
            let (_, resp) = ui.allocate_exact_size(
                vec2(ui.available_width(), empty.y.max(base * 2.0)),
                Sense::click(),
            );
            if resp.double_clicked() {
                out.new_tab = true;
            }
            resp.context_menu(|ui| {
                if ui.button("New tab").clicked() {
                    out.new_tab = true;
                    ui.close();
                }
                if ui.button("New scratchpad").clicked() {
                    out.new_scratchpad = true;
                    ui.close();
                }
                ui.separator();
                if ui.button("Collapse all groups").clicked() {
                    out.collapse_all = true;
                    ui.close();
                }
                if ui.button("Expand all groups").clicked() {
                    out.expand_all = true;
                    ui.close();
                }
            });
        });
    out
}

/// Everything the card would say if it had the width: the row's identity, unabbreviated.
fn tab_tooltip(tab: &TabRow) -> String {
    let mut lines = vec![tab.title.clone()];
    if let Some(sub) = &tab.subtitle {
        lines.push(sub.clone());
    }
    lines.extend(tab.badges.iter().map(|b| b.text.clone()));
    if let Some(git) = &tab.git {
        lines.push(format!("git {}", git.text));
    }
    lines.join("\n")
}

fn draw_tab(
    p: &egui::Painter,
    card: Rect,
    tab: &TabRow,
    colors: &UiColors,
    ty: &Typography,
    time: f64,
    hovered: bool,
) {
    let radius = CornerRadius::same(chrome::R_ROW);
    let key_color = if tab.elevated {
        colors.vermilion
    } else {
        colors.accent
    };
    if tab.active {
        p.rect_filled(card, radius, colors.accent_soft);
    } else if hovered {
        p.rect_filled(card, radius, colors.surface_hi);
    }
    if tab.elevated {
        chrome::outline(p, card, chrome::R_ROW, Stroke::new(1.0, colors.vermilion));
    }
    if tab.active {
        // Left marker bar, inset so it reads as part of the rounded card.
        let bar = Rect::from_min_max(
            pos2(card.left() + 1.0, card.top() + 6.0),
            pos2(card.left() + 4.0, card.bottom() - 6.0),
        );
        p.rect_filled(bar, CornerRadius::same(2), key_color);
    }

    // Keyboard index, right-aligned in its own gutter so the numbers form a column.
    let gutter_right = card.left() + GUTTER;
    let geom = TabGeometry::new(ty.ui.size, ty.small.size, tab.subtitle.is_some());
    let title_cy = match geom.sub_cy {
        Some(_) => card.top() + geom.title_cy,
        None => card.center().y,
    };
    if tab.index <= 9 {
        let g = p.layout_no_wrap(tab.index.to_string(), ty.mono_small.clone(), colors.faint);
        p.galley(
            pos2(gutter_right - g.size().x, title_cy - g.size().y / 2.0),
            g,
            colors.faint,
        );
    }

    // Two marks, two axes: the icon says what this session *is* (and never changes while it
    // lives), the dot says what it is *doing*. Collapsing them into one would make a running
    // container indistinguishable from a running remote at a glance, which is the thing this
    // rail exists to make easy.
    let icon_x = gutter_right + 4.0 + ICON / 2.0;
    chrome::icon(
        p,
        tab.icon,
        pos2(icon_x, title_cy),
        ICON,
        if tab.active {
            tab.icon_color
        } else {
            tab.icon_color.gamma_multiply(0.82)
        },
    );
    let (dot_color, halo) = indicator_color(&tab.indicator, colors, time);
    let dot_x = icon_x + ICON / 2.0 + 8.0;
    let text_x = dot_x + 11.0;
    chrome::dot(p, pos2(dot_x, title_cy), 3.5, dot_color, halo);

    // Line one carries state that needs attention, line two the identity of the session. When
    // there is no second line everything shares the first.
    let mut urgent: Vec<Chip> = Vec::new();
    if tab.bell {
        urgent.push(Chip::new(p, "bell", &ty.micro, colors.amber));
    }
    if let Indicator::Failure(code) = tab.indicator {
        urgent.push(Chip::new(p, &format!("exit {code}"), &ty.micro, colors.red));
    }
    let identity: Vec<Chip> = tab
        .badges
        .iter()
        .map(|b| Chip::new(p, &b.text, &ty.micro, b.color))
        .collect();

    /// Pack chips leftwards from `right`, stopping before they eat the text they share a row
    /// with. Returns the new right edge.
    fn pack(p: &egui::Painter, chips: &[Chip], mut right: f32, cy: f32, floor: f32) -> f32 {
        for chip in chips {
            let w = chip.width();
            if right - w - 6.0 < floor {
                break;
            }
            right -= w;
            chip.paint(p, right, cy);
            right -= 5.0;
        }
        right
    }

    let text_floor = text_x + 90.0;
    let mut title_right = card.right() - 8.0;
    title_right = pack(p, &urgent, title_right, title_cy, text_floor);
    if geom.sub_cy.is_none() {
        title_right = pack(p, &identity, title_right, title_cy, text_floor);
    }

    let (title_font, title_color) = if tab.active {
        (&ty.strong, colors.fg)
    } else {
        (&ty.ui, colors.fg.gamma_multiply(0.78))
    };
    chrome::line(
        p,
        text_x,
        title_cy,
        &tab.title,
        title_font,
        title_color,
        (title_right - text_x).max(0.0),
    );

    if let (Some(sub), Some(sub_cy)) = (&tab.subtitle, geom.sub_cy) {
        let sub_y = card.top() + sub_cy;
        let mut sub_right = card.right() - 8.0;
        let sub_floor = text_x + SUB_FLOOR;
        // Right to left: git branch, then the CPU gauge, then the remaining identity chips,
        // leaving `name · memory` the space that is left. Reading order along the row is
        // therefore name → memory → cpu → branch.
        if let Some(git) = &tab.git {
            // Drawn first, budgeted last: the branch is pinned rightmost but it is the
            // lowest-priority claimant on this line — the others say what the session is
            // doing *now*, the branch only says where it is — so it gets what the gauge, the
            // identity chips and the subtitle's own floor leave behind, and no more. An
            // unbudgeted `Chip::new` here ran a long `feature/…` name off the card and clean
            // out of the rail.
            let mut reserved = if tab.load.is_some() {
                GAUGE_W + 6.0
            } else {
                0.0
            };
            reserved += identity.iter().map(|c| c.width() + 6.0).sum::<f32>();
            let budget = sub_right - reserved - sub_floor;
            // A branch that fits is always shown; the noise floor only gates the ones that
            // have to be cut, because *those* are what stop being worth the pixels.
            let natural = Chip::new(p, &git.text, &ty.micro, git.color);
            let chip = if natural.width() <= budget {
                Some(natural)
            } else if budget >= ty.micro.size * GIT_MIN_EMS {
                Some(Chip::branch(p, &git.text, &ty.micro, git.color, budget))
            } else {
                None
            };
            if let Some(chip) = chip {
                sub_right -= chip.width();
                chip.paint(p, sub_right, sub_y);
                sub_right -= 6.0;
            }
        }
        if let Some((load, reading)) = &tab.load {
            let w = GAUGE_W;
            let h = line_box(ty.small.size).min(GAUGE_H);
            let gauge_rect = Rect::from_min_size(pos2(sub_right - w, sub_y - h / 2.0), vec2(w, h));
            chrome::gauge(
                p,
                gauge_rect,
                *load,
                reading,
                &chrome::GaugeStyle {
                    track: colors.surface_hi,
                    fill: if *load > 0.85 {
                        colors.amber.gamma_multiply(0.55)
                    } else {
                        colors.accent.gamma_multiply(0.45)
                    },
                    font: &ty.micro,
                    text: colors.fg.gamma_multiply(0.9),
                },
            );
            sub_right -= w + 6.0;
        }
        sub_right = pack(p, &identity, sub_right, sub_y, sub_floor);
        chrome::line(
            p,
            text_x,
            sub_y,
            sub,
            &ty.small,
            colors.muted,
            (sub_right - text_x).max(0.0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two text lines of a tab card must not share a baseline: their line boxes have to
    /// clear each other, whatever the font size.
    #[test]
    fn subtitle_line_box_clears_the_title() {
        for base in [9.0_f32, 11.0, 13.0, 18.0, 24.0] {
            let small = (base - 2.0).max(8.0);
            let g = TabGeometry::new(base, small, true);
            let gap = g.sub_cy.expect("two-line card") - g.title_cy;
            let needed = (line_box(base) + line_box(small)) / 2.0;
            assert!(
                gap >= needed,
                "baselines {gap} apart, need {needed} at {base}"
            );
        }
    }

    /// Both lines have to fit inside the card the row reserves for them.
    #[test]
    fn lines_fit_inside_the_row() {
        let g = TabGeometry::new(13.0, 11.0, true);
        let card_h = g.row_h - GAP_Y * 2.0;
        assert!(g.title_cy - line_box(13.0) / 2.0 >= 0.0);
        assert!(g.sub_cy.unwrap() + line_box(11.0) / 2.0 <= card_h);
    }

    /// The hue has to survive a restart, or the colour never becomes part of how a group is
    /// recognised — which is the only reason to colour it.
    #[test]
    fn a_group_keeps_its_hue_across_runs() {
        let keys = [
            "local:/home/rayben/src/verterm",
            "remote:orohost",
            "elevated",
        ];
        let a = group_tints(&keys, GROUP_TINTS);
        let b = group_tints(&keys, GROUP_TINTS);
        assert_eq!(a, b);
        // And it does not depend on what a *later* group is called.
        let mut moved = keys.to_vec();
        moved.push("container:sedanos");
        assert_eq!(group_tints(&moved, GROUP_TINTS)[..3], a[..3]);
    }

    /// Two boxes stacked on each other in the same colour are one box, so neighbours are
    /// always separated even when the hash puts them together.
    #[test]
    fn touching_groups_never_share_a_hue() {
        // Enough keys that collisions are certain with only six hues.
        let owned: Vec<String> = (0..200).map(|i| format!("local:/p/{i}")).collect();
        let keys: Vec<&str> = owned.iter().map(String::as_str).collect();
        let t = group_tints(&keys, GROUP_TINTS);
        for w in t.windows(2) {
            assert_ne!(w[0], w[1], "adjacent groups share a hue");
        }
    }

    #[test]
    fn every_hue_index_is_in_range() {
        let owned: Vec<String> = (0..100).map(|i| format!("remote:host{i}")).collect();
        let keys: Vec<&str> = owned.iter().map(String::as_str).collect();
        assert!(
            group_tints(&keys, GROUP_TINTS)
                .iter()
                .all(|i| *i < GROUP_TINTS)
        );
    }

    /// Degenerate inputs are reachable: a rail with no groups at all (the find view lists tabs
    /// under a heading), and a palette that somehow offers nothing.
    #[test]
    fn tint_assignment_survives_degenerate_input() {
        assert!(group_tints(&[], GROUP_TINTS).is_empty());
        assert_eq!(group_tints(&["a", "b"], 0), vec![0, 0]);
        assert_eq!(group_tints(&["a", "b"], 1), vec![0, 0]);
    }

    #[test]
    fn a_subtitle_makes_the_row_taller() {
        let one = TabGeometry::new(13.0, 11.0, false).row_h;
        let two = TabGeometry::new(13.0, 11.0, true).row_h;
        assert!(two > one);
    }
}

//! The vertical tab rail: bucket → group → tab rows drawn with the painter. Rows have per-kind
//! heights so a tab with a subtitle gets two real text baselines. Pure presentation; the App
//! builds the rows and applies the output.
//!
//! Each bucket (local, remote, containers, elevated) is one rounded box washed with its
//! category hue — the only box in the tree. Inside it the hierarchy is drawn as a *spine*: a
//! thin line in the same hue runs down the box, groups sit on it as hollow nodes and tabs
//! branch off it. Separation between categories comes from the boxes; structure inside a
//! category comes from the line, so groups and tabs need no outlines of their own. Because a
//! box's height is only known after its rows are laid out, it reserves a shape slot before its
//! first row (so it paints *under* them) and fills it in when the span closes — see [`Panel`].

use std::borrow::Cow;
use std::time::Duration;

use egui::{
    Color32, CornerRadius, Rect, Sense, Shape, Stroke, StrokeKind, epaint::RectShape,
    layers::ShapeIdx, pos2, vec2,
};

use super::chrome::{self, Chip, Icon, Typography};
use super::theme::{UiColors, mix, tint};
use crate::session::TabId;

/// Box inset from the rail edges.
const PAD_X: f32 = 8.0;
/// Vertical gap between tab cards.
const GAP_Y: f32 = 1.0;
/// Width of the keyboard-index gutter inside a tab card.
const GUTTER: f32 = 12.0;
/// Side of the kind mark (bucket headers, and tabs listed outside a group).
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
const CARD_PAD_Y: f32 = 5.0;
/// Space between a tab's title and its subtitle.
const LINE_GAP: f32 = 2.0;
/// Corner radius of a bucket box.
const R_BOX: u8 = 10;
/// The spine's distance from the box's left edge. The bucket's icon is centred on it, so the
/// line reads as hanging from the header.
const SPINE_DX: f32 = 16.0;
const SPINE_W: f32 = 1.5;
/// Radius of a group's node on the spine.
const NODE_R: f32 = 3.5;
/// Gap between the spine and the left edge of a tab card; the branch fills it.
const BRANCH: f32 = 12.0;
/// Tab card inset from the box's right edge.
const CARD_INSET_R: f32 = 6.0;
/// Most status dots a collapsed bucket summarises on its header.
const MAX_SUMMARY_DOTS: usize = 6;

// The spine has to sit inside the box with room for a node, and a branch has to be long enough
// to read as one rather than as a stray pixel between the line and the card.
const _: () = assert!(SPINE_DX > NODE_R + 4.0);
const _: () = assert!(BRANCH > NODE_R * 2.0);

/// How strongly a bucket box is washed with its hue: `(fill, edge)` alpha. Elevated is pushed
/// harder so a root session is never one more quiet box in the stack.
fn box_alpha(elevated: bool) -> (u8, u8) {
    if elevated { (20, 66) } else { (13, 41) }
}

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
    /// What kind of session this is. Inside the tree the bucket box already says it, so the
    /// mark is drawn only where a tab is listed outside one (the find view).
    pub icon: Icon,
    /// Colour of that mark: the kind's own colour, not the session's state.
    pub icon_color: Color32,
    /// Second line, left segment: `name · memory`. Only the active tab (and find results)
    /// carry one; every other tab is a single line.
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
        /// The category's own hue — it washes the whole box and draws its spine, so the App
        /// picks it from the bucket rather than the rail re-deriving it from the key string.
        tint: Color32,
        /// States of the tabs inside, summarised on the header while the bucket is collapsed
        /// so a failure or a running job stays visible. Empty when expanded.
        summary: Vec<Indicator>,
    },
    Group {
        key: String,
        name: String,
        collapsed: bool,
        count: usize,
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

/// `0:42`, `12:03`, `1:02:03` — how long the foreground command has been running.
fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Shorten every long path in a title to its last two components: `~/Dropbox/PC/develop/
/// mudengine` → `…/develop/mudengine`. Shell titles lead with the cwd, and a rail-width cut
/// from the right keeps exactly the part that is the same for every tab (`~/Dropbox/PC/…`)
/// and drops the part that tells them apart. The full title stays in the tooltip.
fn compact_title(title: &str) -> Cow<'_, str> {
    let compact = |tok: &str| -> Option<String> {
        if !(tok.starts_with("~/") || tok.starts_with('/')) {
            return None;
        }
        let parts: Vec<&str> = tok.split('/').filter(|p| !p.is_empty()).collect();
        (parts.len() > 3).then(|| format!("…/{}", parts[parts.len() - 2..].join("/")))
    };
    if !title.split(' ').any(|t| compact(t).is_some()) {
        return Cow::Borrowed(title);
    }
    Cow::Owned(
        title
            .split(' ')
            .map(|t| compact(t).unwrap_or_else(|| t.to_string()))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// A bucket box: a container whose height is only known once the rows inside it have been
/// laid out, plus the spine that runs down it.
///
/// egui lays rows out top-down, so a box behind a span of them cannot be painted when the
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
    /// Bottom edge of the last row seen inside this box.
    bottom: f32,
    hue: Color32,
    elevated: bool,
    /// Where the spine starts (under the header) and the centre line of the last row that
    /// branches off it. The line stops there, so the last tab reads as `└` rather than the
    /// spine running on past it.
    spine: Option<(f32, f32)>,
}

impl Panel {
    fn open(
        p: &egui::Painter,
        rect: Rect,
        (left, right): (f32, f32),
        hue: Color32,
        elevated: bool,
    ) -> Self {
        Self {
            slot: p.add(Shape::Noop),
            left,
            right,
            top: rect.top(),
            bottom: rect.bottom(),
            hue,
            elevated,
            spine: None,
        }
    }

    fn spine_x(&self) -> f32 {
        self.left + SPINE_DX
    }

    /// The box's fill composited onto the rail, for shapes that must hide the spine behind
    /// them (a node is hollow, but the line must not show through it).
    fn ground(&self, rail: Color32) -> Color32 {
        let (fill, _) = box_alpha(self.elevated);
        mix(rail, self.hue, fill as f32 / 255.0)
    }

    /// A row that hangs off the spine at height `y`.
    fn branch(&mut self, spine_top: f32, y: f32) {
        self.spine = Some(match self.spine {
            Some((top, _)) => (top, y),
            None => (spine_top, y),
        });
    }

    fn close(self, p: &egui::Painter) {
        // Rows tile without gaps; the padding below the last row is the air inside the box.
        let rect = Rect::from_min_max(
            pos2(self.left, self.top),
            pos2(self.right, (self.bottom + 5.0).max(self.top)),
        );
        let (fill, edge) = box_alpha(self.elevated);
        let mut shapes: Vec<Shape> = vec![
            RectShape::new(
                rect,
                CornerRadius::same(R_BOX),
                tint(self.hue, fill),
                Stroke::new(1.0, tint(self.hue, edge)),
                StrokeKind::Inside,
            )
            .into(),
        ];
        if let Some((top, bottom)) = self.spine {
            let x = self.spine_x();
            let line = Rect::from_min_max(
                pos2(x - SPINE_W / 2.0, top),
                pos2(x + SPINE_W / 2.0, bottom.max(top)),
            );
            shapes.push(RectShape::filled(line, CornerRadius::same(1), tint(self.hue, 97)).into());
        }
        p.set(self.slot, Shape::Vec(shapes));
    }
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
        Row::Bucket { .. } => (ty.ui.size * 2.3).round(),
        Row::Group { .. } => (ty.ui.size * 1.8).round(),
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

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(2.0);
            // The open box. It closes when the next bucket starts or the list ends.
            let mut bucket: Option<Panel> = None;
            // Whether the current run of tabs sits under a group heading (the tree) or
            // directly under a bucket (the find view).
            let mut in_group = false;

            for (i, row) in rows.iter().enumerate() {
                if i > 0 && matches!(row, Row::Bucket { .. }) {
                    ui.add_space(base * 0.6);
                }
                let (rect, resp) = ui.allocate_exact_size(
                    vec2(ui.available_width(), row_height(row, ty)),
                    Sense::click(),
                );
                let span = (rect.left() + PAD_X, rect.right() - PAD_X);

                // Close the box this row ends before opening anything, so a box's slot is
                // always reserved before its own rows.
                if matches!(row, Row::Bucket { .. })
                    && let Some(b) = bucket.take()
                {
                    b.close(ui.painter());
                }
                if let Some(b) = bucket.as_mut() {
                    b.bottom = rect.bottom();
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
                        summary,
                    } => {
                        let hue = if *elevated { colors.vermilion } else { *hue };
                        in_group = false;
                        // The box is reserved on the *unclipped* painter: it will grow past
                        // this row, so clipping it to the header would cut it off.
                        let b = Panel::open(ui.painter(), rect, span, hue, *elevated);
                        let head = Rect::from_min_max(
                            pos2(span.0, rect.top()),
                            pos2(span.1, rect.bottom()),
                        );
                        let cy = head.center().y + 1.0;
                        if resp.hovered() {
                            let r = if *collapsed {
                                CornerRadius::same(R_BOX)
                            } else {
                                CornerRadius {
                                    nw: R_BOX,
                                    ne: R_BOX,
                                    sw: 0,
                                    se: 0,
                                }
                            };
                            painter.rect_filled(head, r, tint(hue, 14));
                        }
                        chrome::icon(&painter, *icon, pos2(b.spine_x(), cy), ICON, hue);
                        chrome::section_label(
                            &painter,
                            pos2(b.spine_x() + ICON / 2.0 + 7.0, cy),
                            label,
                            &ty.micro,
                            hue,
                        );
                        let mut right = head.right() - 12.0;
                        chrome::chevron(&painter, pos2(right, cy), 3.0, !*collapsed, colors.faint);
                        right -= 12.0;
                        let chip =
                            Chip::plain(&painter, &count.to_string(), &ty.micro, colors.faint);
                        right -= chip.width();
                        chip.paint(&painter, right, cy);
                        // A collapsed box still says what its sessions are doing.
                        for ind in summary.iter().take(MAX_SUMMARY_DOTS).rev() {
                            right -= 9.0;
                            let (c, halo) = indicator_color(ind, colors, time);
                            chrome::dot(&painter, pos2(right, cy), 3.0, c, halo);
                        }
                        bucket = Some(b);
                        if resp.clicked() {
                            out.toggle_key = Some(key.clone());
                        }
                    }
                    Row::Group {
                        key,
                        name,
                        collapsed,
                        count,
                    } => {
                        in_group = true;
                        let cy = rect.center().y + 1.0;
                        let (x, hue, ground) = match bucket.as_mut() {
                            Some(b) => {
                                b.branch(rect.top(), cy);
                                (b.spine_x(), b.hue, b.ground(colors.surface))
                            }
                            None => (span.0 + SPINE_DX, colors.muted, colors.surface),
                        };
                        // A node on the spine: hollow while open, filled once collapsed, so a
                        // folded group is visible from the line alone.
                        painter.circle(
                            pos2(x, cy),
                            NODE_R,
                            if *collapsed { hue } else { ground },
                            Stroke::new(1.5, hue),
                        );
                        let text_x = x + NODE_R + 7.0;
                        let mut right = span.1 - CARD_INSET_R - 4.0;
                        if *collapsed || resp.hovered() {
                            let chip =
                                Chip::plain(&painter, &count.to_string(), &ty.micro, colors.faint);
                            right -= chip.width();
                            chip.paint(&painter, right, cy);
                            right -= 6.0;
                        }
                        chrome::line(
                            &painter,
                            text_x,
                            cy,
                            name,
                            &ty.small,
                            if resp.hovered() {
                                colors.fg
                            } else {
                                colors.muted
                            },
                            (right - text_x).max(0.0),
                        );
                        if resp.clicked() {
                            out.toggle_key = Some(key.clone());
                        }
                    }
                    Row::Tab(tab) => {
                        let geom =
                            TabGeometry::new(ty.ui.size, ty.small.size, tab.subtitle.is_some());
                        let card = |x: f32| {
                            Rect::from_min_max(
                                pos2(x, rect.top() + GAP_Y),
                                pos2(span.1 - CARD_INSET_R, rect.bottom() - GAP_Y),
                            )
                        };
                        let (card, branch) = match bucket.as_mut() {
                            Some(b) => {
                                let x = b.spine_x();
                                let card = card(x + BRANCH);
                                let y = card.top() + geom.title_cy;
                                b.branch(rect.top(), y);
                                (card, Some((x, y, b.hue)))
                            }
                            None => (card(span.0 + 4.0), None),
                        };
                        if let Some((x, y, hue)) = branch {
                            let (w, c) = if tab.active {
                                (2.0, colors.accent)
                            } else {
                                (SPINE_W, tint(hue, 97))
                            };
                            painter.line_segment(
                                [pos2(x, y), pos2(card.left() - 2.0, y)],
                                Stroke::new(w, c),
                            );
                        }
                        draw_tab(
                            &painter,
                            card,
                            tab,
                            colors,
                            ty,
                            time,
                            resp.hovered(),
                            !in_group,
                        );
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
                        // The row can only ever show a compacted title, a fitted branch and
                        // a packed subset of the chips; hovering is the way back to what they
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

#[allow(clippy::too_many_arguments)]
fn draw_tab(
    p: &egui::Painter,
    card: Rect,
    tab: &TabRow,
    colors: &UiColors,
    ty: &Typography,
    time: f64,
    hovered: bool,
    show_icon: bool,
) {
    let radius = CornerRadius::same(chrome::R_ROW - 2);
    if tab.active {
        p.rect_filled(card, radius, colors.surface_hi);
    } else if hovered {
        p.rect_filled(card, radius, tint(colors.fg, 12));
    }

    let geom = TabGeometry::new(ty.ui.size, ty.small.size, tab.subtitle.is_some());
    let title_cy = card.top() + geom.title_cy;

    // Keyboard index, right-aligned in its own gutter so the numbers form a column.
    let gutter_right = card.left() + 6.0 + GUTTER;
    if tab.index <= 9 {
        let c = if tab.active {
            colors.accent
        } else {
            colors.faint
        };
        let g = p.layout_no_wrap(tab.index.to_string(), ty.mono_small.clone(), c);
        p.galley(
            pos2(gutter_right - g.size().x, title_cy - g.size().y / 2.0),
            g,
            c,
        );
    }
    let mut text_x = gutter_right + 8.0;
    if show_icon {
        chrome::icon(
            p,
            tab.icon,
            pos2(text_x + ICON / 2.0, title_cy),
            ICON,
            tab.icon_color,
        );
        text_x += ICON + 6.0;
    }

    // Right to left along the title line: the state dot, then whatever needs attention — a
    // failure code, the running time, a bell.
    let (dot_color, halo) = indicator_color(&tab.indicator, colors, time);
    let mut right = card.right() - 9.0;
    chrome::dot(p, pos2(right - 3.5, title_cy), 3.5, dot_color, halo);
    right -= 7.0 + 7.0;
    let mut urgent: Vec<Chip> = Vec::new();
    match tab.indicator {
        Indicator::Failure(code) => {
            urgent.push(Chip::plain(p, &format!("✕{code}"), &ty.micro, colors.red))
        }
        Indicator::Running { since } if since >= Duration::from_secs(1) => urgent.push(
            Chip::plain(p, &format_elapsed(since), &ty.micro, colors.faint),
        ),
        _ => {}
    }
    if tab.bell {
        urgent.push(Chip::new(p, "bell", &ty.micro, colors.amber));
    }

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
    let title_right = pack(p, &urgent, right, title_cy, text_x + 60.0);

    let title_color = if tab.elevated {
        colors.vermilion
    } else if tab.active {
        colors.fg
    } else {
        colors.fg.gamma_multiply(0.72)
    };
    let title_font = if tab.active { &ty.strong } else { &ty.ui };
    chrome::line(
        p,
        text_x,
        title_cy,
        &compact_title(&tab.title),
        title_font,
        title_color,
        (title_right - text_x).max(0.0),
    );

    if let (Some(sub), Some(sub_cy)) = (&tab.subtitle, geom.sub_cy) {
        let identity: Vec<Chip> = tab
            .badges
            .iter()
            .map(|b| Chip::new(p, &b.text, &ty.micro, b.color))
            .collect();
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
                    track: colors.surface_alt,
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
            colors.faint,
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

    #[test]
    fn a_subtitle_makes_the_row_taller() {
        let one = TabGeometry::new(13.0, 11.0, false).row_h;
        let two = TabGeometry::new(13.0, 11.0, true).row_h;
        assert!(two > one);
    }

    /// The part of a cwd title that tells tabs apart is its tail; the shared prefix goes.
    #[test]
    fn long_paths_in_a_title_keep_their_tail() {
        assert_eq!(
            compact_title("~/Dropbox/PC/develop/mudengine"),
            "…/develop/mudengine"
        );
        assert_eq!(
            compact_title("/usr/local/share/verterm: cargo build"),
            "…/share/verterm: cargo build"
        );
        assert_eq!(
            compact_title("nvim ~/src/a/b/c/main.rs"),
            "nvim …/c/main.rs"
        );
    }

    /// Short paths and plain titles are already as good as they get.
    #[test]
    fn short_titles_are_left_alone() {
        for t in [
            "~ - fish",
            "~/src/verterm",
            "/etc/nginx",
            "sudo top",
            "",
            "a/b/c/d/e",
        ] {
            assert!(matches!(compact_title(t), Cow::Borrowed(_)), "{t}");
        }
    }

    #[test]
    fn elapsed_time_reads_like_a_clock() {
        assert_eq!(format_elapsed(Duration::from_secs(5)), "0:05");
        assert_eq!(format_elapsed(Duration::from_secs(42)), "0:42");
        assert_eq!(format_elapsed(Duration::from_secs(12 * 60 + 3)), "12:03");
        assert_eq!(format_elapsed(Duration::from_secs(3723)), "1:02:03");
    }

    /// Root has to stand out from the stack of quiet boxes, by fill and by edge.
    #[test]
    fn the_elevated_box_is_washed_harder() {
        let (f, e) = box_alpha(false);
        let (rf, re) = box_alpha(true);
        assert!(rf > f && re > e);
    }
}

//! The vertical tab rail: bucket → group → tab rows drawn as cards with the painter. Rows have
//! per-kind heights so a tab with a subtitle gets two real text baselines. Pure presentation;
//! the App builds the rows and applies the output.

use std::time::Duration;

use egui::{Color32, CornerRadius, Rect, Sense, Stroke, pos2, vec2};

use super::chrome::{self, Chip, Icon, Typography};
use super::theme::UiColors;
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

    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(4.0);
            for (i, row) in rows.iter().enumerate() {
                // Buckets open a new section; give them air unless they lead the list.
                if i > 0 && matches!(row, Row::Bucket { .. }) {
                    ui.add_space(base * 0.6);
                }
                let (rect, resp) = ui.allocate_exact_size(
                    vec2(ui.available_width(), row_height(row, ty)),
                    Sense::click(),
                );
                let painter = ui.painter().with_clip_rect(rect);
                let card = Rect::from_min_max(
                    pos2(rect.left() + PAD_X, rect.top() + GAP_Y),
                    pos2(rect.right() - PAD_X, rect.bottom() - GAP_Y),
                );
                match row {
                    Row::Bucket {
                        key,
                        label,
                        icon,
                        collapsed,
                        count,
                        elevated,
                    } => {
                        let color = if *elevated {
                            colors.vermilion
                        } else {
                            colors.faint
                        };
                        chrome::chevron(
                            &painter,
                            pos2(card.left() + 5.0, card.center().y),
                            3.5,
                            !*collapsed,
                            color,
                        );
                        chrome::icon(
                            &painter,
                            *icon,
                            pos2(card.left() + 17.0, card.center().y),
                            ICON - 1.0,
                            color,
                        );
                        chrome::section_label(
                            &painter,
                            pos2(card.left() + 28.0, card.center().y),
                            label,
                            &ty.micro,
                            color,
                        );
                        let chip =
                            Chip::plain(&painter, &count.to_string(), &ty.micro, colors.faint);
                        chip.paint(&painter, card.right() - chip.width(), card.center().y);
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
                        if resp.hovered() {
                            painter.rect_filled(
                                card,
                                CornerRadius::same(chrome::R_ROW),
                                colors.surface_hi,
                            );
                        }
                        let color = if *elevated {
                            colors.vermilion
                        } else {
                            colors.muted
                        };
                        chrome::chevron(
                            &painter,
                            pos2(card.left() + 13.0, card.center().y),
                            3.5,
                            !*collapsed,
                            colors.faint,
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
                            pos2(card.left() + 26.0, card.center().y),
                            ICON,
                            color,
                        );
                        let x = card.left() + 36.0;
                        // A group is a header for the tabs under it, so it reads at the same
                        // size as their titles; `ty.small` made the heading quieter than its
                        // own children and inverted the hierarchy.
                        chrome::line(
                            &painter,
                            x,
                            card.center().y,
                            name,
                            &ty.ui,
                            color,
                            (right - x).max(0.0),
                        );
                        if resp.clicked() {
                            out.toggle_key = Some(key.clone());
                        }
                    }
                    Row::Tab(tab) => {
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

    #[test]
    fn a_subtitle_makes_the_row_taller() {
        let one = TabGeometry::new(13.0, 11.0, false).row_h;
        let two = TabGeometry::new(13.0, 11.0, true).row_h;
        assert!(two > one);
    }
}

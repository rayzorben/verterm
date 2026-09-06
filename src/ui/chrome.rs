//! The small vocabulary of shapes every piece of chrome is drawn from: cards, chips, status
//! dots, meters and section labels, plus the type scale. Pure painting — nothing here reads or
//! mutates app state, so the rail, status bar and overlays stay consistent by construction.

use std::sync::Arc;

use egui::{
    Align2, Color32, CornerRadius, FontId, Frame, Galley, Margin, Painter, Pos2, Rect, Stroke,
    StrokeKind, epaint::Shadow, pos2, vec2,
};

use super::fonts::TermFonts;
use super::theme::{UiColors, tint};

/// Corner radius of a floating card (overlays, toasts).
pub const R_CARD: u8 = 12;
/// Corner radius of a list row or tab card.
pub const R_ROW: u8 = 8;
/// Corner radius of an input well or code block.
pub const R_WELL: u8 = 7;
/// Padding inside a chip, per side.
const CHIP_PAD_X: f32 = 6.0;
const CHIP_PAD_Y: f32 = 2.0;

/// Drop shadow shared by every floating surface.
pub fn shadow() -> Shadow {
    Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(120),
    }
}

/// The type scale. Chrome is proportional; anything the user could copy, type or compare
/// column-wise (commands, paths, dimensions) stays monospace.
#[derive(Clone, Debug)]
pub struct Typography {
    /// Body text in the chrome — tab titles, palette rows.
    pub ui: FontId,
    /// The same size in the medium cut, for titles and active rows.
    pub strong: FontId,
    /// Secondary line: subtitles, hints, footers.
    pub small: FontId,
    /// Micro caps: bucket headers, chip text.
    pub micro: FontId,
    /// Code: commands, paths, grid dimensions.
    pub mono: FontId,
    /// Code, one step down.
    pub mono_small: FontId,
}

impl Typography {
    pub fn new(fonts: &TermFonts, base: f32) -> Self {
        Self {
            ui: fonts.ui_id(base, false),
            strong: fonts.ui_id(base, true),
            small: fonts.ui_id((base - 2.0).max(8.0), false),
            micro: fonts.ui_id((base - 3.0).max(7.0), true),
            mono: fonts.id(base - 1.0, false, false),
            mono_small: fonts.id((base - 2.5).max(7.0), false, false),
        }
    }
}

/// Frame for a floating surface: filled, hairlined, rounded and shadowed.
pub fn card_frame(c: &UiColors) -> Frame {
    Frame::new()
        .fill(c.float)
        .stroke(Stroke::new(1.0, c.border_strong))
        .inner_margin(Margin::same(12))
        .corner_radius(CornerRadius::same(R_CARD))
        .shadow(shadow())
}

/// Frame for an inset well — text inputs and code previews inside a card.
pub fn well_frame(c: &UiColors) -> Frame {
    Frame::new()
        .fill(c.surface_alt)
        .stroke(Stroke::new(1.0, c.border))
        .inner_margin(Margin::symmetric(8, 5))
        .corner_radius(CornerRadius::same(R_WELL))
}

/// A rounded, tinted label. Laid out once on construction so it can be measured before it is
/// placed — the rail packs chips from the right edge, the status bar from the left.
pub struct Chip {
    galley: Arc<Galley>,
    fg: Color32,
    bg: Option<Color32>,
}

impl Chip {
    /// Tinted background, text in `color` — the default for a badge.
    pub fn new(p: &Painter, text: &str, font: &FontId, color: Color32) -> Self {
        Self {
            galley: p.layout_no_wrap(text.to_string(), font.clone(), color),
            fg: color,
            bg: Some(tint(color, 34)),
        }
    }

    /// No background: plain text that still participates in chip layout.
    pub fn plain(p: &Painter, text: &str, font: &FontId, color: Color32) -> Self {
        Self {
            galley: p.layout_no_wrap(text.to_string(), font.clone(), color),
            fg: color,
            bg: None,
        }
    }

    /// Solid background with an explicit text colour — for the one chip that must shout.
    pub fn solid(p: &Painter, text: &str, font: &FontId, fg: Color32, bg: Color32) -> Self {
        Self {
            galley: p.layout_no_wrap(text.to_string(), font.clone(), fg),
            fg,
            bg: Some(bg),
        }
    }

    pub fn width(&self) -> f32 {
        self.galley.size().x
            + if self.bg.is_some() {
                CHIP_PAD_X * 2.0
            } else {
                0.0
            }
    }

    pub fn height(&self) -> f32 {
        self.galley.size().y + CHIP_PAD_Y * 2.0
    }

    /// Paint with the left edge at `left`, vertically centred on `cy`. Returns the width used.
    pub fn paint(&self, p: &Painter, left: f32, cy: f32) -> f32 {
        let w = self.width();
        if let Some(bg) = self.bg {
            let rect =
                Rect::from_min_size(pos2(left, cy - self.height() / 2.0), vec2(w, self.height()));
            p.rect_filled(rect, CornerRadius::same(R_WELL), bg);
        }
        let tx = left + if self.bg.is_some() { CHIP_PAD_X } else { 0.0 };
        p.galley(
            pos2(tx, cy - self.galley.size().y / 2.0),
            self.galley.clone(),
            self.fg,
        );
        w
    }
}

/// A status dot, optionally wrapped in a soft halo (used for "running").
pub fn dot(p: &Painter, center: Pos2, radius: f32, color: Color32, halo: bool) {
    if halo {
        p.circle_filled(center, radius * 2.1, tint(color, 46));
    }
    p.circle_filled(center, radius, color);
}

/// A meter tall enough to carry its own reading: a rounded track, a proportional fill, and the
/// value centred on top of both.
///
/// The fill is deliberately soft. The label crosses the boundary between filled and unfilled
/// as the value climbs, so a saturated bar would leave the text unreadable at exactly the
/// loads worth reading; a washed fill keeps one text colour legible over the whole range.
pub struct GaugeStyle<'a> {
    pub track: Color32,
    pub fill: Color32,
    pub font: &'a FontId,
    pub text: Color32,
}

pub fn gauge(p: &Painter, rect: Rect, frac: f32, label: &str, st: &GaugeStyle<'_>) {
    let (track, fill, font, text) = (st.track, st.fill, st.font, st.text);
    let r = CornerRadius::same((rect.height() / 2.0).round() as u8);
    p.rect_filled(rect, r, track);
    let w = rect.width() * frac.clamp(0.0, 1.0);
    if w >= rect.height() {
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(w, rect.height())),
            r,
            fill,
        );
    } else if w > 0.0 {
        // Narrower than the cap radius: a rounded rect would render as a dot in the wrong
        // place, so draw a square sliver flush with the left cap instead.
        p.rect_filled(
            Rect::from_min_size(rect.min, vec2(w.max(1.0), rect.height())),
            CornerRadius::ZERO,
            fill,
        );
    }
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        font.clone(),
        text,
    );
}

/// Hairline across the full width of `rect` at its top edge.
pub fn top_hairline(p: &Painter, rect: Rect, color: Color32) {
    p.line_segment(
        [
            pos2(rect.left(), rect.top() + 0.5),
            pos2(rect.right(), rect.top() + 0.5),
        ],
        Stroke::new(1.0, color),
    );
}

/// Rounded outline drawn just inside `rect`.
pub fn outline(p: &Painter, rect: Rect, radius: u8, stroke: Stroke) {
    p.rect_stroke(rect, CornerRadius::same(radius), stroke, StrokeKind::Inside);
}

/// A disclosure chevron drawn from two strokes — pointing down when `open`, right when not.
/// Drawn rather than typed so it never depends on the UI font carrying a triangle glyph.
pub fn chevron(p: &Painter, center: Pos2, half: f32, open: bool, color: Color32) {
    let s = Stroke::new(1.3, color);
    if open {
        let (dx, dy) = (half, half * 0.5);
        p.line_segment(
            [
                pos2(center.x - dx, center.y - dy),
                pos2(center.x, center.y + dy),
            ],
            s,
        );
        p.line_segment(
            [
                pos2(center.x, center.y + dy),
                pos2(center.x + dx, center.y - dy),
            ],
            s,
        );
    } else {
        let (dx, dy) = (half * 0.5, half);
        p.line_segment(
            [
                pos2(center.x - dx, center.y - dy),
                pos2(center.x + dx, center.y),
            ],
            s,
        );
        p.line_segment(
            [
                pos2(center.x + dx, center.y),
                pos2(center.x - dx, center.y + dy),
            ],
            s,
        );
    }
}

/// What a rail row *is*, drawn as a small mark so the kind of a session is recognisable
/// without reading it. One shape per bucket, plus the two local shapes that are worth
/// telling apart at a glance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// A local session whose group is a project directory.
    Folder,
    /// A local session sitting in `$HOME` (or with no project root).
    Home,
    /// An SSH / mosh / telnet session.
    Globe,
    /// A session inside a local container.
    Box,
    /// A root / sudo session.
    Shield,
    /// An ephemeral scratchpad.
    Bolt,
}

/// Draw `icon` centred on `center`, sized to fit a `size`-wide box.
///
/// Drawn from strokes and rects rather than typed, for the same reason [`chevron`] is: the UI
/// font is whatever `fc-match` resolved for `[font].ui_family` (Noto Sans here) and carries no
/// icon glyphs, and depending on a Nerd Font being the *grid* font would make the rail render
/// tofu for anyone who picked a plain monospace face. These also stay crisp at 12 px, which a
/// scaled-down glyph does not.
pub fn icon(p: &Painter, icon: Icon, center: Pos2, size: f32, color: Color32) {
    let h = size / 2.0;
    let s = Stroke::new(1.4, color);
    let (cx, cy) = (center.x, center.y);
    match icon {
        Icon::Folder => {
            // A tab along the top-left, then the body.
            let top = cy - h * 0.68;
            let body =
                Rect::from_min_max(pos2(cx - h, top + h * 0.34), pos2(cx + h, cy + h * 0.72));
            p.line_segment(
                [pos2(cx - h, body.top()), pos2(cx - h * 0.25, body.top())],
                s,
            );
            p.line_segment(
                [pos2(cx - h * 0.25, body.top()), pos2(cx - h * 0.05, top)],
                s,
            );
            p.line_segment([pos2(cx - h * 0.05, top), pos2(cx + h, top)], s);
            p.rect_stroke(
                Rect::from_min_max(pos2(cx - h, top), pos2(cx + h, body.bottom())),
                CornerRadius::same(2),
                s,
                StrokeKind::Inside,
            );
        }
        Icon::Home => {
            // Roof, then the walls.
            let roof = cy - h;
            p.line_segment([pos2(cx - h, cy - h * 0.1), pos2(cx, roof)], s);
            p.line_segment([pos2(cx, roof), pos2(cx + h, cy - h * 0.1)], s);
            p.line_segment(
                [pos2(cx - h * 0.72, cy), pos2(cx - h * 0.72, cy + h * 0.8)],
                s,
            );
            p.line_segment(
                [pos2(cx + h * 0.72, cy), pos2(cx + h * 0.72, cy + h * 0.8)],
                s,
            );
            p.line_segment(
                [
                    pos2(cx - h * 0.72, cy + h * 0.8),
                    pos2(cx + h * 0.72, cy + h * 0.8),
                ],
                s,
            );
        }
        Icon::Globe => {
            // Circle plus one meridian and one parallel — enough to read as a globe at 12 px.
            p.circle_stroke(center, h * 0.92, s);
            p.line_segment([pos2(cx - h * 0.92, cy), pos2(cx + h * 0.92, cy)], s);
            // The meridian is an ellipse; two arcs would cost a path, so approximate with a
            // narrow vertical lens made of two line pairs.
            let q = h * 0.42;
            p.line_segment([pos2(cx, cy - h * 0.92), pos2(cx - q, cy)], s);
            p.line_segment([pos2(cx - q, cy), pos2(cx, cy + h * 0.92)], s);
            p.line_segment([pos2(cx, cy - h * 0.92), pos2(cx + q, cy)], s);
            p.line_segment([pos2(cx + q, cy), pos2(cx, cy + h * 0.92)], s);
        }
        Icon::Box => {
            // An isometric crate: top face plus the vertical seam.
            let top = cy - h * 0.9;
            let mid = cy - h * 0.35;
            let bot = cy + h * 0.9;
            p.line_segment([pos2(cx - h, mid), pos2(cx, top)], s);
            p.line_segment([pos2(cx, top), pos2(cx + h, mid)], s);
            p.line_segment([pos2(cx - h, mid), pos2(cx - h, cy + h * 0.35)], s);
            p.line_segment([pos2(cx + h, mid), pos2(cx + h, cy + h * 0.35)], s);
            p.line_segment([pos2(cx - h, cy + h * 0.35), pos2(cx, bot)], s);
            p.line_segment([pos2(cx, bot), pos2(cx + h, cy + h * 0.35)], s);
            p.line_segment([pos2(cx, top), pos2(cx, bot)], s);
        }
        Icon::Shield => {
            // Flat shoulders narrowing to a point — the one shape that should read as "careful".
            let top = cy - h * 0.95;
            let shoulder = cy + h * 0.1;
            p.line_segment([pos2(cx - h * 0.85, top), pos2(cx + h * 0.85, top)], s);
            p.line_segment([pos2(cx - h * 0.85, top), pos2(cx - h * 0.85, shoulder)], s);
            p.line_segment([pos2(cx + h * 0.85, top), pos2(cx + h * 0.85, shoulder)], s);
            p.line_segment([pos2(cx - h * 0.85, shoulder), pos2(cx, cy + h * 0.95)], s);
            p.line_segment([pos2(cx + h * 0.85, shoulder), pos2(cx, cy + h * 0.95)], s);
        }
        Icon::Bolt => {
            let s = Stroke::new(1.3, color);
            p.line_segment(
                [pos2(cx + h * 0.5, cy - h), pos2(cx - h * 0.5, cy + h * 0.1)],
                s,
            );
            p.line_segment(
                [
                    pos2(cx - h * 0.5, cy + h * 0.1),
                    pos2(cx + h * 0.15, cy + h * 0.1),
                ],
                s,
            );
            p.line_segment(
                [
                    pos2(cx + h * 0.15, cy + h * 0.1),
                    pos2(cx - h * 0.5, cy + h),
                ],
                s,
            );
        }
    }
}

/// The verterm mark: a window split into a tab rail and a terminal pane, with one accented
/// tab and a shell prompt.
///
/// The same drawing as `assets/verterm.svg` (which is what the desktop entry and the launcher
/// use), redrawn here from primitives rather than loaded as an image — for the reason the rest
/// of this module is drawn: no image decoder, no `egui_extras`, no asset to keep in sync with
/// the theme, and it stays crisp at whatever size the rail's font scale asks for.
pub fn brand(p: &Painter, center: Pos2, size: f32, fg: Color32, accent: Color32) {
    // Everything below is expressed on the SVG's 128-unit grid, so the two cannot drift.
    let u = size / 128.0;
    let at = |x: f32, y: f32| pos2(center.x + (x - 64.0) * u, center.y + (y - 64.0) * u);
    let frame = Stroke::new((4.0 * u).max(1.0), fg);

    // Window + the rail divider that makes it this terminal rather than any terminal.
    p.rect_stroke(
        Rect::from_min_max(at(24.0, 28.0), at(104.0, 100.0)),
        CornerRadius::same((8.0 * u).round().max(1.0) as u8),
        frame,
        StrokeKind::Inside,
    );
    p.line_segment([at(48.0, 28.0), at(48.0, 100.0)], frame);

    // Three tabs; the first is active.
    let tab = Stroke::new((4.0 * u).max(1.0), accent);
    let idle = Stroke::new((4.0 * u).max(1.0), fg.gamma_multiply(0.45));
    p.line_segment([at(33.0, 42.0), at(40.0, 42.0)], tab);
    p.line_segment([at(33.0, 58.0), at(40.0, 58.0)], idle);
    p.line_segment([at(33.0, 74.0), at(40.0, 74.0)], idle);

    // The prompt: chevron plus caret.
    let pen = Stroke::new((4.5 * u).max(1.0), fg);
    p.line_segment([at(60.0, 52.0), at(70.0, 62.0)], pen);
    p.line_segment([at(70.0, 62.0), at(60.0, 72.0)], pen);
    p.line_segment([at(78.0, 72.0), at(92.0, 72.0)], pen);
}

/// A micro-caps section label, e.g. the rail's bucket headers.
pub fn section_label(p: &Painter, at: Pos2, text: &str, font: &FontId, color: Color32) {
    p.text(at, Align2::LEFT_CENTER, text, font.clone(), color);
}

/// Lay out a single line truncated to `max_width` with an ellipsis.
pub fn elide(
    p: &Painter,
    text: &str,
    font: &FontId,
    color: Color32,
    max_width: f32,
) -> Arc<Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font.clone(), color);
    job.wrap = egui::text::TextWrapping {
        max_width: max_width.max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    p.layout_job(job)
}

/// Paint an elided line with its left edge at `left`, vertically centred on `cy`.
pub fn line(
    p: &Painter,
    left: f32,
    cy: f32,
    text: &str,
    font: &FontId,
    color: Color32,
    max_width: f32,
) {
    let g = elide(p, text, font, color, max_width);
    p.galley(pos2(left, cy - g.size().y / 2.0), g, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_is_soft_and_downward() {
        let s = shadow();
        assert_eq!(s.offset, [0, 8]);
        assert!(s.blur > s.spread);
    }
}

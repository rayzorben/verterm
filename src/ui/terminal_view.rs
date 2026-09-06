//! Paint an alacritty grid with the egui painter and translate pointer positions back into
//! grid coordinates. Text is batched into per-row runs of identical style; every non-ASCII
//! or wide glyph is positioned individually so cell alignment never drifts.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::Match;
use alacritty_terminal::term::{Term, TermMode, point_to_viewport, viewport_to_point};
use alacritty_terminal::vte::ansi::{CursorShape, NamedColor};
use egui::{Align2, Color32, CornerRadius, Painter, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};

use super::fonts::{CellMetrics, TermFonts};
use super::theme::{Palette, UiColors, to_color32};
use crate::hints::{HintMatch, RowText};
use crate::session::EventProxy;

pub struct HintOverlay<'a> {
    pub matches: &'a [HintMatch],
    pub tags: &'a [String],
    pub typed: &'a str,
}

pub struct RenderCtx<'a> {
    pub palette: &'a Palette,
    pub colors: &'a UiColors,
    pub fonts: &'a TermFonts,
    pub font_size: f32,
    pub metrics: CellMetrics,
    pub focused: bool,
    /// False during the "off" half of a blink cycle.
    pub cursor_visible: bool,
    pub hints: Option<HintOverlay<'a>>,
    pub search_match: Option<&'a Match>,
}

struct Run {
    row: usize,
    col: usize,
    next_col: usize,
    text: String,
    fg: Color32,
    bold: bool,
    italic: bool,
    deco: Flags,
}

const DECO_MASK: Flags = Flags::ALL_UNDERLINES.union(Flags::STRIKEOUT);

pub fn render(painter: &Painter, rect: Rect, term: &Term<EventProxy>, rc: &RenderCtx<'_>) {
    let content = term.renderable_content();
    let display_offset = content.display_offset;
    let mode = content.mode;
    let colors = content.colors;
    let m = rc.metrics;
    let origin = rect.min;

    let default_bg = to_color32(rc.palette.named(NamedColor::Background, colors));
    painter.rect_filled(rect, CornerRadius::ZERO, default_bg);

    let text_dy = ((m.h - m.text_h) / 2.0).max(0.0);
    let cell_pos = |row: usize, col: usize| -> Pos2 {
        pos2(origin.x + col as f32 * m.w, origin.y + row as f32 * m.h)
    };

    let mut run: Option<Run> = None;
    let flush = |run: &mut Option<Run>| {
        if let Some(r) = run.take() {
            if r.text.trim().is_empty() && !r.deco.intersects(DECO_MASK) {
                return;
            }
            let pos = cell_pos(r.row, r.col);
            let width = (r.next_col - r.col) as f32 * m.w;
            painter.text(
                pos + vec2(0.0, text_dy),
                Align2::LEFT_TOP,
                &r.text,
                rc.fonts.id(rc.font_size, r.bold, r.italic),
                r.fg,
            );
            draw_decorations(painter, pos, width, m.h, r.deco, r.fg);
        }
    };

    for indexed in content.display_iter {
        let cell = indexed.cell;
        let point = indexed.point;
        let Some(vp) = point_to_viewport(display_offset, point) else {
            continue;
        };
        let (row, col) = (vp.line, vp.column.0);
        let flags = cell.flags;
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            continue;
        }
        let wide = flags.contains(Flags::WIDE_CHAR);

        let (mut fg_c, mut bg_c) = (cell.fg, cell.bg);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg_c, &mut bg_c);
        }
        let fg = to_color32(rc.palette.resolve_fg(fg_c, flags, colors));
        let mut bg = to_color32(rc.palette.resolve(bg_c, colors));
        if content
            .selection
            .as_ref()
            .is_some_and(|s| s.contains(point))
        {
            bg = rc.colors.selection;
        }
        if rc.search_match.is_some_and(|mm| mm.contains(&point)) {
            bg = rc.colors.search;
        }

        let width = if wide { 2.0 } else { 1.0 } * m.w;
        let cell_rect = Rect::from_min_size(cell_pos(row, col), vec2(width, m.h));
        if bg != default_bg {
            painter.rect_filled(cell_rect, CornerRadius::ZERO, bg);
        }
        if flags.contains(Flags::HIDDEN) {
            flush(&mut run);
            continue;
        }

        let c = cell.c;
        let bold = flags.contains(Flags::BOLD);
        let italic = flags.contains(Flags::ITALIC);
        let deco = flags & DECO_MASK;

        if !c.is_ascii() || wide || cell.zerowidth().is_some() {
            flush(&mut run);
            let mut text = String::new();
            text.push(c);
            if let Some(zw) = cell.zerowidth() {
                text.extend(zw);
            }
            painter.text(
                cell_rect.min + vec2(0.0, text_dy),
                Align2::LEFT_TOP,
                text,
                rc.fonts.id(rc.font_size, bold, italic),
                fg,
            );
            draw_decorations(painter, cell_rect.min, width, m.h, deco, fg);
            continue;
        }

        let contiguous = run.as_ref().is_some_and(|r| {
            r.row == row
                && r.next_col == col
                && r.fg == fg
                && r.bold == bold
                && r.italic == italic
                && r.deco == deco
        });
        if contiguous {
            let r = run.as_mut().unwrap();
            r.text.push(c);
            r.next_col += 1;
        } else {
            flush(&mut run);
            if c != ' ' || deco.intersects(DECO_MASK) {
                run = Some(Run {
                    row,
                    col,
                    next_col: col + 1,
                    text: c.to_string(),
                    fg,
                    bold,
                    italic,
                    deco,
                });
            }
        }
    }
    flush(&mut run);

    // Cursor -------------------------------------------------------------------------
    let cursor = content.cursor;
    let vi_mode = mode.contains(TermMode::VI);
    if (mode.contains(TermMode::SHOW_CURSOR) || vi_mode)
        && cursor.shape != CursorShape::Hidden
        && let Some(vp) = point_to_viewport(display_offset, cursor.point)
    {
        let cell = &term.grid()[cursor.point];
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        let width = if wide { 2.0 } else { 1.0 } * m.w;
        let r = Rect::from_min_size(cell_pos(vp.line, vp.column.0), vec2(width, m.h));
        let cursor_color = to_color32(rc.palette.named(NamedColor::Cursor, colors));
        let shape = if rc.focused {
            cursor.shape
        } else {
            CursorShape::HollowBlock
        };
        if rc.cursor_visible || !rc.focused {
            match shape {
                CursorShape::Block => {
                    painter.rect_filled(r, CornerRadius::ZERO, cursor_color);
                    let text_color = to_color32(rc.palette.resolve(
                        if cell.flags.contains(Flags::INVERSE) {
                            cell.fg
                        } else {
                            cell.bg
                        },
                        colors,
                    ));
                    let mut text = String::new();
                    text.push(cell.c);
                    if let Some(zw) = cell.zerowidth() {
                        text.extend(zw);
                    }
                    if cell.c != ' ' {
                        painter.text(
                            r.min + vec2(0.0, text_dy),
                            Align2::LEFT_TOP,
                            text,
                            rc.fonts.id(rc.font_size, false, false),
                            text_color,
                        );
                    }
                }
                CursorShape::Beam => {
                    painter.rect_filled(
                        Rect::from_min_size(r.min, vec2(2.0, m.h)),
                        CornerRadius::ZERO,
                        cursor_color,
                    );
                }
                CursorShape::Underline => {
                    painter.rect_filled(
                        Rect::from_min_size(pos2(r.min.x, r.max.y - 2.0), vec2(width, 2.0)),
                        CornerRadius::ZERO,
                        cursor_color,
                    );
                }
                CursorShape::HollowBlock => {
                    painter.rect_stroke(
                        r,
                        CornerRadius::ZERO,
                        Stroke::new(1.0, cursor_color),
                        StrokeKind::Inside,
                    );
                }
                CursorShape::Hidden => {}
            }
        }
    }

    // Hints ---------------------------------------------------------------------------
    if let Some(h) = &rc.hints {
        let font = rc.fonts.id(rc.font_size, true, false);
        for (hm, tag) in h.matches.iter().zip(h.tags.iter()) {
            if !tag.starts_with(h.typed) {
                continue;
            }
            let pos = cell_pos(hm.row, hm.col_start);
            let w = (hm.col_end - hm.col_start) as f32 * m.w;
            painter.rect_stroke(
                Rect::from_min_size(pos, vec2(w, m.h)),
                CornerRadius::ZERO,
                Stroke::new(1.0, rc.colors.hint_bg),
                StrokeKind::Inside,
            );
            let tag_w = tag.chars().count() as f32 * m.w + 4.0;
            let tag_rect = Rect::from_min_size(pos, vec2(tag_w, m.h));
            painter.rect_filled(tag_rect, CornerRadius::same(2), rc.colors.hint_bg);
            painter.text(
                tag_rect.min + vec2(2.0, text_dy),
                Align2::LEFT_TOP,
                tag,
                font.clone(),
                rc.colors.hint_fg,
            );
        }
    }
}

fn draw_decorations(
    painter: &Painter,
    pos: Pos2,
    width: f32,
    height: f32,
    deco: Flags,
    color: Color32,
) {
    if deco.is_empty() {
        return;
    }
    let x0 = pos.x;
    let x1 = pos.x + width;
    let base = pos.y + height - 1.5;
    let stroke = Stroke::new(1.0, color);
    if deco.contains(Flags::DOUBLE_UNDERLINE) {
        painter.line_segment([pos2(x0, base - 2.0), pos2(x1, base - 2.0)], stroke);
        painter.line_segment([pos2(x0, base), pos2(x1, base)], stroke);
    } else if deco.contains(Flags::UNDERCURL) {
        // Approximate the curl with a dashed line so it is visibly different from underline.
        let mut x = x0;
        while x < x1 {
            painter.line_segment([pos2(x, base), pos2((x + 2.0).min(x1), base - 1.0)], stroke);
            x += 4.0;
        }
    } else if deco.contains(Flags::DOTTED_UNDERLINE) {
        let mut x = x0;
        while x < x1 {
            painter.line_segment([pos2(x, base), pos2((x + 1.0).min(x1), base)], stroke);
            x += 3.0;
        }
    } else if deco.contains(Flags::DASHED_UNDERLINE) {
        let mut x = x0;
        while x < x1 {
            painter.line_segment([pos2(x, base), pos2((x + 4.0).min(x1), base)], stroke);
            x += 7.0;
        }
    } else if deco.contains(Flags::UNDERLINE) {
        painter.line_segment([pos2(x0, base), pos2(x1, base)], stroke);
    }
    if deco.contains(Flags::STRIKEOUT) {
        let y = pos.y + height / 2.0;
        painter.line_segment([pos2(x0, y), pos2(x1, y)], stroke);
    }
}

/// Text of every visible row with a char → column mapping (for hints).
/// The hint (URL / path / IP / hash) under a pointer position, if any. Used by the terminal
/// context menu so "Open" works without entering hint mode; scans just the clicked row.
pub fn hint_at(
    pos: Pos2,
    rect: Rect,
    metrics: CellMetrics,
    term: &Term<EventProxy>,
) -> Option<HintMatch> {
    let (point, _) = pos_to_point(pos, rect, metrics, term);
    let vp = point_to_viewport(term.grid().display_offset(), point)?;
    let row = viewport_rows(term).into_iter().nth(vp.line)?;
    let col = point.column.0;
    crate::hints::find_hints(std::slice::from_ref(&row))
        .into_iter()
        .find(|h| h.col_start <= col && col < h.col_end)
        .map(|mut h| {
            h.row = vp.line;
            h
        })
}

/// Select the whole visible viewport (context-menu "Select all"). Grid coordinates stay in
/// this module so `ui::mod` never has to reason about display offsets.
pub fn select_viewport(term: &mut Term<EventProxy>) -> Option<Selection> {
    let lines = term.grid().screen_lines();
    let cols = term.grid().columns();
    if lines == 0 || cols == 0 {
        return None;
    }
    let top = viewport_to_point(term.grid().display_offset(), Point::new(0, Column(0)));
    let bottom = viewport_to_point(
        term.grid().display_offset(),
        Point::new(lines - 1, Column(cols - 1)),
    );
    let mut sel = Selection::new(SelectionType::Simple, top, Side::Left);
    sel.update(bottom, Side::Right);
    Some(sel)
}

pub fn viewport_rows(term: &Term<EventProxy>) -> Vec<RowText> {
    let content = term.renderable_content();
    let lines = term.grid().screen_lines();
    let mut rows = vec![RowText::default(); lines];
    for indexed in content.display_iter {
        let Some(vp) = point_to_viewport(content.display_offset, indexed.point) else {
            continue;
        };
        let cell = indexed.cell;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        if let Some(row) = rows.get_mut(vp.line) {
            row.push(cell.c, vp.column.0);
            if let Some(zw) = cell.zerowidth() {
                for &z in zw {
                    row.push(z, vp.column.0);
                }
            }
        }
    }
    for r in &mut rows {
        r.trim_end();
    }
    rows
}

/// The last `n` non-empty lines up to and including the cursor line (AI context).
pub fn last_lines(term: &Term<EventProxy>, n: usize) -> Vec<String> {
    let grid = term.grid();
    let cursor_line = grid.cursor.point.line.0;
    let top = grid.topmost_line().0;
    let last_col = grid.last_column();
    let mut out = Vec::new();
    let mut line = cursor_line;
    while line >= top && out.len() < n {
        let text = term.bounds_to_string(
            Point::new(Line(line), Column(0)),
            Point::new(Line(line), last_col),
        );
        let trimmed = text.trim_end();
        if !trimmed.trim().is_empty() {
            out.push(trimmed.to_string());
        }
        line -= 1;
    }
    out.reverse();
    out
}

/// Text of the grid lines between two scroll-invariant line numbers (see
/// `session::GridPos::abs`), as the head and tail of the range with a `…` between them when
/// something was dropped. Blank rows are skipped.
///
/// Only `head + tail` rows are ever read, and the scan from each end is bounded, so a command
/// that printed 10 000 lines costs the same as one that printed three.
pub fn output_excerpt(
    term: &Term<EventProxy>,
    from_abs: i64,
    to_abs: i64,
    head: usize,
    tail: usize,
) -> Vec<String> {
    /// Rows examined from each end while looking for non-blank ones.
    const SCAN: i64 = 200;
    let grid = term.grid();
    let hist = grid.history_size() as i64;
    let last_col = grid.last_column();
    // Lines evicted from the scrollback are gone; clamp rather than index out of the grid.
    let top = grid.topmost_line().0 as i64;
    let bottom = grid.bottommost_line().0 as i64;
    let lo = (from_abs - hist).clamp(top, bottom);
    let hi = (to_abs - hist).clamp(top, bottom);
    if hi < lo {
        return Vec::new();
    }
    let read = |l: i64| {
        let l = Line(l as i32);
        term.bounds_to_string(Point::new(l, Column(0)), Point::new(l, last_col))
            .trim_end()
            .to_string()
    };

    let mut front = Vec::new();
    let mut i = lo;
    while i <= hi && front.len() < head && i - lo < SCAN {
        let text = read(i);
        if !text.trim().is_empty() {
            front.push(text);
        }
        i += 1;
    }
    let mut back = Vec::new();
    let mut j = hi;
    while j >= i && back.len() < tail && hi - j < SCAN {
        let text = read(j);
        if !text.trim().is_empty() {
            back.push(text);
        }
        j -= 1;
    }
    back.reverse();
    if j >= i {
        front.push("…".to_string());
    }
    front.extend(back);
    front
}

/// Pointer position → grid point (respecting scrollback offset) and which half was hit.
pub fn pos_to_point(
    pos: Pos2,
    rect: Rect,
    metrics: CellMetrics,
    term: &Term<EventProxy>,
) -> (Point, Side) {
    let grid = term.grid();
    let cols = grid.columns().max(1);
    let lines = grid.screen_lines().max(1);
    let rel_x = (pos.x - rect.left()).max(0.0);
    let rel_y = (pos.y - rect.top()).max(0.0);
    let col = ((rel_x / metrics.w) as usize).min(cols - 1);
    let row = ((rel_y / metrics.h) as usize).min(lines - 1);
    let point = viewport_to_point(grid.display_offset(), Point::new(row, Column(col)));
    let side = if rel_x % metrics.w < metrics.w / 2.0 {
        Side::Left
    } else {
        Side::Right
    };
    (point, side)
}

/// Viewport row of the cursor (for overlay placement).
pub fn cursor_row(term: &Term<EventProxy>) -> usize {
    let grid = term.grid();
    (grid.cursor.point.line.0 + grid.display_offset() as i32).max(0) as usize
}

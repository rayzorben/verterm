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
use crate::config::LinkDecor;
use crate::hints::{HintMatch, RowText};
use crate::links::Matcher;
use crate::session::EventProxy;

pub struct HintOverlay<'a> {
    pub matches: &'a [HintMatch],
    pub tags: &'a [String],
    pub typed: &'a str,
}

/// A run of cells on one grid row, in viewport coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    pub row: usize,
    pub col_start: usize,
    /// Exclusive.
    pub col_end: usize,
}

impl LinkSpan {
    fn overlaps(self, other: LinkSpan) -> bool {
        self.row == other.row && self.col_start < other.col_end && self.col_end > other.col_start
    }
}

/// A hyperlink placed on the visible grid.
///
/// One link, not one row of one: a URL longer than the terminal is wide wraps, and the halves
/// are two spans of the *same* link. Modelling them as two links would underline them as two
/// and — much worse — open the truncated first half on a click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GridLink {
    /// One entry per grid row the link occupies, top to bottom. Never empty.
    pub spans: Vec<LinkSpan>,
    /// What is on screen. For OSC 8 this is the *label*, which the program is free to make say
    /// anything at all — which is why it is never what gets opened, and why the UI shows `uri`.
    pub text: String,
    /// What the opener is handed.
    pub uri: String,
    /// From OSC 8 rather than from detection.
    pub explicit: bool,
}

impl GridLink {
    /// Where the link starts, for ordering and for anchoring a hint tag.
    pub fn head(&self) -> LinkSpan {
        self.spans[0]
    }

    fn hits(&self, row: usize, col: usize) -> bool {
        self.spans
            .iter()
            .any(|s| s.row == row && s.col_start <= col && col < s.col_end)
    }

    fn overlaps(&self, other: &GridLink) -> bool {
        self.spans
            .iter()
            .any(|a| other.spans.iter().any(|b| a.overlaps(*b)))
    }
}

pub struct LinkOverlay<'a> {
    pub links: &'a [GridLink],
    /// Index into `links` of the one under the pointer.
    pub hovered: Option<usize>,
    pub underline: LinkDecor,
    pub color: LinkDecor,
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
    pub links: Option<LinkOverlay<'a>>,
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

    // Link styling is a per-cell question asked once per cell, so it gets a flat mask rather
    // than a search through the link list: 0 = not a link, 1 = link, 2 = the hovered link.
    // (A viewport is a few thousand cells and there may be dozens of links; scanning the list
    // per cell is the one shape of this that would actually cost something.)
    let cols = term.grid().columns();
    let link_mask: Vec<u8> = match &rc.links {
        Some(l) if !l.links.is_empty() && cols > 0 => {
            let lines = term.grid().screen_lines();
            let mut mask = vec![0u8; cols * lines];
            for (i, g) in l.links.iter().enumerate() {
                let v = if l.hovered == Some(i) { 2 } else { 1 };
                // Every span, so hovering either half of a wrapped link lights up both.
                for span in &g.spans {
                    if span.row >= lines {
                        continue;
                    }
                    for c in span.col_start..span.col_end.min(cols) {
                        mask[span.row * cols + c] = v;
                    }
                }
            }
            mask
        }
        _ => Vec::new(),
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
        let mut fg = to_color32(rc.palette.resolve_fg(fg_c, flags, colors));
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
        let mut deco = flags & DECO_MASK;

        // Underline is additive and colour is not, which is why the shipped default underlines
        // always but only recolours on hover: overwriting the foreground the program chose
        // destroys meaning (ls colours, a diff, a log level) for the sake of a cue the
        // underline already gives. Both are `[hyperlinks]` keys; neither is baked in.
        if let Some(l) = &rc.links
            && let Some(&state) = link_mask.get(row * cols + col)
            && state != 0
        {
            let hovered = state == 2;
            if l.underline.shows(hovered) {
                deco |= Flags::UNDERLINE;
            }
            if l.color.shows(hovered) {
                fg = rc.colors.link;
            }
        }

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

/// The hint (URL / path / IP / hash) under a pointer position, if any. Used by the terminal
/// context menu so "Open" works without entering hint mode; scans just the clicked row.
pub fn hint_at(
    pos: Pos2,
    rect: Rect,
    metrics: CellMetrics,
    term: &Term<EventProxy>,
    links: &Matcher,
) -> Option<HintMatch> {
    let (point, _) = pos_to_point(pos, rect, metrics, term);
    let vp = point_to_viewport(term.grid().display_offset(), point)?;
    let row = viewport_rows(term).into_iter().nth(vp.line)?;
    let col = point.column.0;
    crate::hints::find_hints(std::slice::from_ref(&row), links)
        .into_iter()
        .find(|h| h.col_start <= col && col < h.col_end)
        .map(|mut h| {
            h.row = vp.line;
            h
        })
}

/// Which link a pointer position lands on, as an index into `links`. Pure grid arithmetic —
/// no `Term`, because link rows are already viewport rows.
pub fn link_at(links: &[GridLink], pos: Pos2, rect: Rect, metrics: CellMetrics) -> Option<usize> {
    if !rect.contains(pos) || metrics.w <= 0.0 || metrics.h <= 0.0 {
        return None;
    }
    let col = ((pos.x - rect.left()) / metrics.w) as usize;
    let row = ((pos.y - rect.top()) / metrics.h) as usize;
    links.iter().position(|l| l.hits(row, col))
}

/// Every hyperlink on the visible grid.
///
/// One walk of the display collects both halves: the OSC 8 runs the program declared cell by
/// cell, and the row text detection needs. Explicit links win any overlap — a program that
/// took the trouble to emit OSC 8 has said what it means, and re-deriving a link from the
/// label it chose would be second-guessing it (the label need not even be a URL).
///
/// Detection runs over *logical* lines, not grid rows: a row that ends in `WRAPLINE` continues
/// into the next one, and a URL longer than the terminal is wide is the ordinary case, not the
/// exotic one. Scanning rows would find two fragments, underline them as two links and open a
/// truncated URI on a click.
pub fn viewport_links(term: &Term<EventProxy>, links: &Matcher, detect: bool) -> Vec<GridLink> {
    struct Run {
        id: String,
        uri: String,
        spans: Vec<LinkSpan>,
        text: String,
    }

    let content = term.renderable_content();
    let display_offset = content.display_offset;
    let lines = term.grid().screen_lines();
    let mut rows = vec![RowText::default(); lines];
    // `wraps[r]` — row `r` runs on into row `r + 1`.
    let mut wraps = vec![false; lines];
    let mut out: Vec<GridLink> = Vec::new();
    let mut run: Option<Run> = None;

    // A run is closed with its trailing blanks dropped: a program that sets a hyperlink and
    // then erases to end of line leaves the erased cells carrying the template's link, which
    // would otherwise stretch it across the rest of the row.
    fn close(run: Option<Run>, out: &mut Vec<GridLink>) {
        let Some(mut r) = run else { return };
        let trimmed = r.text.trim_end();
        if trimmed.is_empty() {
            return;
        }
        let dropped = r.text.chars().count() - trimmed.chars().count();
        if let Some(last) = r.spans.last_mut() {
            last.col_end -= dropped.min(last.col_end - last.col_start);
        }
        r.spans.retain(|s| s.col_end > s.col_start);
        if r.spans.is_empty() {
            return;
        }
        out.push(GridLink {
            spans: r.spans,
            text: trimmed.to_string(),
            uri: r.uri,
            explicit: true,
        });
    }

    for indexed in content.display_iter {
        let Some(vp) = point_to_viewport(display_offset, indexed.point) else {
            continue;
        };
        let cell = indexed.cell;
        let (row, col) = (vp.line, vp.column.0);
        if cell.flags.contains(Flags::WRAPLINE) && row + 1 < lines {
            wraps[row] = true;
        }
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        if let Some(r) = rows.get_mut(row) {
            r.push(cell.c, col);
            if let Some(zw) = cell.zerowidth() {
                for &z in zw {
                    r.push(z, col);
                }
            }
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        match cell.hyperlink() {
            // Identity is the id, not the uri: two adjacent `ESC ] 8` runs to the same target
            // are two links, and alacritty gives each its own generated id when the program
            // sent none. A run continues into the next cell, or over a wrap into column 0 of
            // the next row; anywhere else it is a new link that happens to share an id.
            Some(h) => {
                let follows = run.as_ref().and_then(|r| r.spans.last()).is_some_and(|s| {
                    (s.row == row && s.col_end == col) || (s.row + 1 == row && col == 0)
                });
                match run.as_mut() {
                    Some(r) if follows && r.id == h.id() => {
                        let last = r.spans.last_mut().expect("`follows` implies a span");
                        if last.row == row {
                            last.col_end = col + width;
                        } else {
                            r.spans.push(LinkSpan {
                                row,
                                col_start: col,
                                col_end: col + width,
                            });
                        }
                        r.text.push(cell.c);
                    }
                    _ => {
                        close(run.take(), &mut out);
                        run = Some(Run {
                            id: h.id().to_string(),
                            uri: h.uri().to_string(),
                            spans: vec![LinkSpan {
                                row,
                                col_start: col,
                                col_end: col + width,
                            }],
                            text: cell.c.to_string(),
                        });
                    }
                }
            }
            None => close(run.take(), &mut out),
        }
    }
    close(run.take(), &mut out);
    out.retain(|l| links.allows(&l.uri));

    if detect {
        for l in detect_links(&rows, &wraps, links) {
            if !out.iter().any(|e| e.overlaps(&l)) {
                out.push(l);
            }
        }
    }
    out.sort_by_key(|l| (l.head().row, l.head().col_start));
    out
}

/// Run the matcher over each logical line (one or more wrapped grid rows) and map the char
/// offsets it reports back onto grid spans.
fn detect_links(rows: &[RowText], wraps: &[bool], links: &Matcher) -> Vec<GridLink> {
    let mut out = Vec::new();
    let mut first = 0usize;
    while first < rows.len() {
        let mut last = first;
        while last + 1 < rows.len() && wraps.get(last).copied().unwrap_or(false) {
            last += 1;
        }
        // One logical line: the wrapped rows joined, with each char remembering where it came
        // from. Only the final row is trimmed — a wrapped row ran off the end and has no
        // trailing blanks to lose.
        let mut text = String::new();
        let mut at: Vec<(usize, usize)> = Vec::new();
        for (r, row) in rows.iter().enumerate().take(last + 1).skip(first) {
            let keep = if r == last {
                row.text.trim_end().chars().count()
            } else {
                row.text.chars().count()
            };
            for (i, c) in row.text.chars().take(keep).enumerate() {
                text.push(c);
                at.push((r, row.cols.get(i).copied().unwrap_or(i)));
            }
        }
        if !text.is_empty() {
            for lm in links.find(&text) {
                let mut spans: Vec<LinkSpan> = Vec::new();
                for &(row, col) in &at[lm.start..lm.end.min(at.len())] {
                    match spans.last_mut() {
                        Some(s) if s.row == row => s.col_end = col + 1,
                        _ => spans.push(LinkSpan {
                            row,
                            col_start: col,
                            col_end: col + 1,
                        }),
                    }
                }
                if !spans.is_empty() {
                    out.push(GridLink {
                        spans,
                        text: lm.text,
                        uri: lm.uri,
                        explicit: false,
                    });
                }
            }
        }
        first = last + 1;
    }
    out
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

/// Text of every visible row with a char → column mapping (for hints).
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

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn link(spans: &[(usize, usize, usize)]) -> GridLink {
        GridLink {
            spans: spans
                .iter()
                .map(|&(row, col_start, col_end)| LinkSpan {
                    row,
                    col_start,
                    col_end,
                })
                .collect(),
            text: "x".into(),
            uri: "https://example.com/x".into(),
            explicit: false,
        }
    }

    fn metrics() -> CellMetrics {
        CellMetrics {
            w: 10.0,
            h: 20.0,
            text_h: 16.0,
        }
    }

    fn rect() -> Rect {
        Rect::from_min_size(pos2(100.0, 50.0), vec2(800.0, 400.0))
    }

    /// Hit-testing is what decides both which link lights up under the pointer and which one a
    /// click opens, so off-by-one at either edge of a cell opens the wrong thing.
    #[test]
    fn a_pointer_hits_the_link_whose_cells_it_is_over() {
        let links = [link(&[(0, 4, 9)]), link(&[(2, 0, 3)])];
        let (r, m) = (rect(), metrics());
        let at = |col: f32, row: f32| {
            link_at(
                &links,
                pos2(r.left() + col * m.w, r.top() + row * m.h),
                r,
                m,
            )
        };
        // First cell of the first link, last cell of it, and one past the end.
        assert_eq!(at(4.0, 0.0), Some(0));
        assert_eq!(at(8.9, 0.5), Some(0));
        assert_eq!(at(9.0, 0.0), None);
        // The column before it, and the same columns on a row with no link.
        assert_eq!(at(3.9, 0.0), None);
        assert_eq!(at(5.0, 1.0), None);
        assert_eq!(at(0.0, 2.0), Some(1));
    }

    /// A wrapped link is one link with two spans, so either half must resolve to the same
    /// index — otherwise clicking the tail would open nothing and clicking the head would open
    /// a truncated URI.
    #[test]
    fn both_halves_of_a_wrapped_link_are_the_same_link() {
        let links = [link(&[(3, 70, 80), (4, 0, 12)])];
        let (r, m) = (rect(), metrics());
        let at = |col: f32, row: f32| {
            link_at(
                &links,
                pos2(r.left() + col * m.w, r.top() + row * m.h),
                r,
                m,
            )
        };
        assert_eq!(at(72.0, 3.0), Some(0));
        assert_eq!(at(5.0, 4.0), Some(0));
        assert_eq!(at(69.0, 3.0), None);
        assert_eq!(at(12.0, 4.0), None);
    }

    #[test]
    fn a_pointer_outside_the_grid_hits_nothing() {
        let links = [link(&[(0, 0, 80)])];
        let (r, m) = (rect(), metrics());
        assert_eq!(
            link_at(&links, pos2(r.left() - 1.0, r.top() + 5.0), r, m),
            None
        );
        assert_eq!(
            link_at(&links, pos2(r.left() + 5.0, r.top() - 1.0), r, m),
            None
        );
        assert_eq!(
            link_at(&links, pos2(r.right() + 1.0, r.top() + 5.0), r, m),
            None
        );
    }

    /// Degenerate metrics happen for a frame at startup, before the fonts have been measured;
    /// dividing by them would put the pointer in column `inf`.
    #[test]
    fn zero_sized_cells_hit_nothing_instead_of_panicking() {
        let links = [link(&[(0, 0, 10)])];
        let m = CellMetrics {
            w: 0.0,
            h: 0.0,
            text_h: 0.0,
        };
        assert_eq!(link_at(&links, rect().center(), rect(), m), None);
    }

    #[test]
    fn an_empty_link_list_hits_nothing() {
        assert_eq!(link_at(&[], rect().center(), rect(), metrics()), None);
    }

    fn row_text(s: &str) -> RowText {
        let mut r = RowText::default();
        for (i, c) in s.chars().enumerate() {
            r.push(c, i);
        }
        r
    }

    fn spans(l: &GridLink) -> Vec<(usize, usize, usize)> {
        l.spans
            .iter()
            .map(|s| (s.row, s.col_start, s.col_end))
            .collect()
    }

    /// The case a per-row scan gets wrong, and gets wrong dangerously: a URL longer than the
    /// terminal is wide. Row-at-a-time detection yields two fragments, and clicking the first
    /// opens a truncated address.
    #[test]
    fn a_url_that_wraps_is_one_link_with_a_span_per_row() {
        let rows = [
            row_text("see https://example.com/very/long"),
            row_text("/path/to/thing after"),
        ];
        let found = detect_links(&rows, &[true, false], &Matcher::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].uri, "https://example.com/very/long/path/to/thing");
        assert_eq!(spans(&found[0]), vec![(0, 4, 33), (1, 0, 14)]);
    }

    /// The mirror image: two rows that merely follow each other must never be joined, or the
    /// scan invents links that are not on screen out of adjacent unrelated output.
    #[test]
    fn unwrapped_rows_are_never_joined() {
        let rows = [row_text("https://example.com"), row_text("/etc/hosts")];
        let found = detect_links(&rows, &[false, false], &Matcher::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].uri, "https://example.com");
        assert_eq!(spans(&found[0]), vec![(0, 0, 19)]);
    }

    /// A wrapped row has no trailing blanks to lose, but the last row of a logical line does —
    /// and keeping them would push the match past the end of the text.
    #[test]
    fn only_the_last_row_of_a_logical_line_is_trimmed() {
        let rows = [
            row_text("go to https://example.com/a"),
            row_text("bc              "),
        ];
        let found = detect_links(&rows, &[true, false], &Matcher::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "https://example.com/abc");
        assert_eq!(spans(&found[0]), vec![(0, 6, 27), (1, 0, 2)]);
    }
}

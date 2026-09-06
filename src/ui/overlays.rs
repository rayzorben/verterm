//! Keyboard-driven overlays: the AI command prompt, the vi search bar, the command palette
//! and the scratchpad command prompt. Each `show_*` returns an outcome; the App applies it.
//! Nothing here writes to a PTY.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

use egui::{
    Align2, Area, CornerRadius, Id, Key, Order, Rect, RichText, Stroke, StrokeKind, TextEdit, pos2,
    vec2,
};

use super::chrome::{self, Chip, Typography};
use super::fonts::CellMetrics;
use super::theme::{UiColors, tint};
use crate::ai::{AiMsg, extract_command};
use crate::config::AiPosition;
use crate::keymap::{Action, Keymap};

// ---------------------------------------------------------------------------------------
// AI overlay
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiPhase {
    Editing,
    Streaming,
    Ready,
}

pub struct AiOverlayState {
    pub input: String,
    pub phase: AiPhase,
    pub raw: String,
    pub command: Option<String>,
    pub explanation: String,
    pub error: Option<String>,
    pub rx: Option<Receiver<AiMsg>>,
    pub cancel: Arc<AtomicBool>,
    pub last_sent: String,
    /// Viewport row of the prompt when the overlay opened, for `Place::Prompt`.
    pub anchor_row: usize,
    /// Where the card sits. Starts from the remembered drag if there is one, else from
    /// `[ai].position`; a drag replaces it with a fraction, which is why dragging a
    /// prompt-anchored overlay detaches it from the prompt.
    pub place: Place,
    /// Card size measured at the end of the previous frame. The first frame has to estimate,
    /// and every later frame places the card from its real size — which is also what keeps a
    /// bottom-anchored card pinned to the bottom as the reply grows.
    pub size: Option<egui::Vec2>,
    /// Grid selection captured when the overlay opened. Sampled there, not at submit time,
    /// because the selection is cleared by `leave_mode` and by the next click — by the time
    /// the user has finished typing a question it may well be gone.
    pub selection: Option<String>,
    /// Whether that selection rides along with the request. On by default when there is one.
    pub include_selection: bool,
    /// The question behind the last suggestion the user *ran*, offered again when that
    /// command failed. Re-submitting it is not a repeat: the request now carries a `[LAST]`
    /// block naming the command and its exit code, so the model diagnoses instead of guessing
    /// the same answer twice.
    pub retry: Option<String>,
}

/// Everything the App knows when the overlay opens.
pub struct AiOpen {
    pub anchor_row: usize,
    pub selection: Option<String>,
    pub position: AiPosition,
    /// Remembered drag, as a fraction of the free space; overrides `position`.
    pub saved_frac: Option<(f32, f32)>,
    pub retry: Option<String>,
}

/// How the card is positioned this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// Fraction of the free space inside the terminal area — a corner, the centre, or wherever
    /// the user dragged it.
    Frac((f32, f32)),
    /// Floating next to the shell prompt and following it as output scrolls.
    Prompt,
}

/// Gap kept between the card and the edge of the terminal area.
const CARD_MARGIN: f32 = 12.0;

/// Top-left of a card of `size` at `frac` of the free space inside `area`.
///
/// The fraction is of the **free** space (area minus the card minus both margins), not of the
/// area, which is what makes a remembered position survive a resize: `(1, 1)` is the
/// bottom-right corner at every window size, and a card too large to fit lands flush against
/// the top-left instead of hanging off the screen.
pub fn place_card(area: Rect, size: egui::Vec2, frac: (f32, f32)) -> egui::Pos2 {
    let free = (area.size() - size - egui::Vec2::splat(2.0 * CARD_MARGIN)).max(egui::Vec2::ZERO);
    pos2(
        area.left() + CARD_MARGIN + free.x * frac.0.clamp(0.0, 1.0),
        area.top() + CARD_MARGIN + free.y * frac.1.clamp(0.0, 1.0),
    )
}

/// Inverse of [`place_card`]: which fraction a card at `pos` represents, clamped so a drag
/// can never park the card outside the terminal area.
pub fn card_fraction(area: Rect, size: egui::Vec2, pos: egui::Pos2) -> (f32, f32) {
    let free = (area.size() - size - egui::Vec2::splat(2.0 * CARD_MARGIN)).max(egui::Vec2::ZERO);
    let f = |v: f32, free: f32| {
        if free > 0.0 {
            (v / free).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    (
        f(pos.x - area.left() - CARD_MARGIN, free.x),
        f(pos.y - area.top() - CARD_MARGIN, free.y),
    )
}

impl AiOverlayState {
    pub fn new(open: AiOpen) -> Self {
        let place = match open.saved_frac.or_else(|| open.position.fraction()) {
            Some(f) => Place::Frac(f),
            None => Place::Prompt,
        };
        Self {
            input: String::new(),
            phase: AiPhase::Editing,
            raw: String::new(),
            command: None,
            explanation: String::new(),
            error: None,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            last_sent: String::new(),
            anchor_row: open.anchor_row,
            place,
            size: None,
            include_selection: open.selection.is_some(),
            selection: open.selection,
            retry: open.retry,
        }
    }

    /// The selection to send with the next request, honouring the toggle.
    pub fn selection_payload(&self) -> Option<String> {
        self.include_selection
            .then(|| self.selection.clone())
            .flatten()
    }

    pub fn begin_request(&mut self, rx: Receiver<AiMsg>, cancel: Arc<AtomicBool>) {
        self.cancel.store(true, Ordering::Relaxed); // abort any previous stream
        self.cancel = cancel;
        self.rx = Some(rx);
        self.raw.clear();
        self.command = None;
        self.explanation.clear();
        self.error = None;
        self.phase = AiPhase::Streaming;
        self.last_sent = self.input.clone();
    }

    pub fn abort(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.rx = None;
    }

    fn poll(&mut self) {
        let Some(rx) = &self.rx else { return };
        loop {
            match rx.try_recv() {
                Ok(AiMsg::Token(t)) => self.raw.push_str(&t),
                Ok(AiMsg::Done) => {
                    self.rx = None;
                    self.phase = if self.raw.trim().is_empty() {
                        AiPhase::Editing
                    } else {
                        AiPhase::Ready
                    };
                    if self.raw.trim().is_empty() {
                        self.error = Some("empty response from model".into());
                    }
                    break;
                }
                Ok(AiMsg::Error(e)) => {
                    self.rx = None;
                    self.error = Some(e);
                    self.phase = AiPhase::Editing;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    self.phase = if self.raw.trim().is_empty() {
                        AiPhase::Editing
                    } else {
                        AiPhase::Ready
                    };
                    break;
                }
            }
        }
        let (cmd, expl) = extract_command(&self.raw);
        self.command = cmd;
        self.explanation = expl;
    }
}

/// Shorten a remembered question for a one-line footer.
fn elide_words(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!(
        "{}…",
        s.chars().take(max - 1).collect::<String>().trim_end()
    )
}

/// Label for the send-selection toggle: enough to tell at a glance how much would leave the
/// machine. Blank lines are not counted — a selection dragged past the end of the output is
/// mostly empty rows and "42 lines" would badly overstate it.
pub fn selection_label(sel: &str) -> String {
    let lines = sel.lines().filter(|l| !l.trim().is_empty()).count();
    let chars = sel.chars().count();
    format!(
        "send selection · {lines} line{}, {chars} char{}",
        if lines == 1 { "" } else { "s" },
        if chars == 1 { "" } else { "s" }
    )
}

/// The send-selection toggle. Drawn from the chrome vocabulary rather than `ui.checkbox` so it
/// matches the card it sits in, and so the tick is strokes rather than a glyph the UI font may
/// not carry. Returns true when clicked.
fn selection_toggle(
    ui: &mut egui::Ui,
    on: bool,
    label: &str,
    c: &UiColors,
    ty: &Typography,
) -> bool {
    let h = (ty.small.size * 2.0).round();
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), egui::Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(chrome::R_ROW), c.surface_hi);
    }
    let side = (ty.small.size * 1.1).round();
    let bx = Rect::from_center_size(
        pos2(rect.left() + 6.0 + side * 0.5, rect.center().y),
        vec2(side, side),
    );
    let radius = CornerRadius::same(3);
    if on {
        p.rect_filled(bx, radius, c.accent);
        let s = Stroke::new(1.6, c.bg);
        let w = bx.width();
        let mid = pos2(bx.left() + w * 0.42, bx.bottom() - w * 0.26);
        p.line_segment([pos2(bx.left() + w * 0.22, bx.top() + w * 0.52), mid], s);
        p.line_segment([mid, pos2(bx.right() - w * 0.18, bx.top() + w * 0.26)], s);
    } else {
        p.rect_stroke(
            bx,
            radius,
            Stroke::new(1.0, c.border_strong),
            StrokeKind::Inside,
        );
    }
    let left = bx.right() + 8.0;
    chrome::line(
        p,
        left,
        rect.center().y,
        label,
        &ty.small,
        if on { c.fg } else { c.faint },
        (rect.right() - left - 8.0).max(0.0),
    );
    resp.clicked()
}

pub enum AiOutcome {
    None,
    Submit(String),
    Insert(String),
    Execute(String),
    /// The user finished dragging the card; the App persists the new fraction.
    Moved((f32, f32)),
    Close,
}

/// Was `key` pressed this frame with Ctrl held?
///
/// The modifier is read off the key event itself rather than `InputState::modifiers`, which
/// is the state left at the *end* of the frame: a chord whose press and release both land in
/// one frame — a slow frame, or synthetic input — leaves `modifiers` already back at NONE and
/// the chord silently does nothing. This is exactly how `Ctrl+S` failed to toggle the
/// selection and how `Ctrl+Enter` could degrade into a plain `Enter`.
pub fn ctrl_pressed(events: &[egui::Event], key: Key) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            egui::Event::Key {
                key: k,
                pressed: true,
                modifiers,
                ..
            } if *k == key && modifiers.ctrl
        )
    })
}

pub struct AiChrome<'a> {
    pub colors: &'a UiColors,
    pub ty: &'a Typography,
    pub metrics: CellMetrics,
    pub model: &'a str,
    pub endpoint: &'a str,
}

pub fn show_ai(
    ctx: &egui::Context,
    term_rect: Rect,
    state: &mut AiOverlayState,
    ch: &AiChrome<'_>,
) -> AiOutcome {
    state.poll();
    let (esc, enter, ctrl_enter, toggle_sel, retry_key) = ctx.input(|i| {
        (
            i.key_pressed(Key::Escape),
            i.key_pressed(Key::Enter),
            ctrl_pressed(&i.events, Key::Enter),
            ctrl_pressed(&i.events, Key::S),
            ctrl_pressed(&i.events, Key::R),
        )
    });
    if toggle_sel && state.selection.is_some() {
        state.include_selection = !state.include_selection;
    }
    let c = ch.colors;
    let ty = ch.ty;
    let m = ch.metrics;

    let width = (term_rect.width() - 2.0 * CARD_MARGIN)
        .min(m.w * 100.0)
        .max(340.0);
    let preview_lines = state
        .command
        .as_deref()
        .map(|s| s.lines().count())
        .unwrap_or(0) as f32;
    let expl_lines = if state.explanation.is_empty() {
        0.0
    } else {
        2.0
    };
    let sel_lines = if state.selection.is_some() { 1.0 } else { 0.0 };
    let est_h = m.h * (4.5 + preview_lines + expl_lines + sel_lines) + 64.0;
    // Real size once measured; the estimate only ever governs the very first frame.
    let size = state.size.unwrap_or(vec2(width + 26.0, est_h));
    let pos = match state.place {
        Place::Frac(f) => place_card(term_rect, size, f),
        Place::Prompt => {
            let prompt_y = term_rect.top() + state.anchor_row as f32 * m.h;
            if prompt_y - term_rect.top() > size.y + 8.0 {
                pos2(term_rect.left() + CARD_MARGIN, prompt_y - size.y - 6.0)
            } else {
                pos2(
                    term_rect.left() + CARD_MARGIN,
                    (prompt_y + m.h + 6.0)
                        .min(term_rect.bottom() - size.y - 6.0)
                        .max(term_rect.top() + 6.0),
                )
            }
        }
    };

    let mut outcome = AiOutcome::None;
    let mut text_changed = false;
    let mut toggle_clicked = false;
    let mut drag_delta = vec2(0.0, 0.0);
    let mut drag_stopped = false;
    let area = Area::new(Id::new("verterm-ai-overlay"))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            chrome::card_frame(c)
                .stroke(Stroke::new(1.0, tint(c.accent, 110)))
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
                    let header = ui.horizontal(|ui| {
                        ui.label(RichText::new("ASK").font(ty.micro.clone()).color(c.accent));
                        ui.label(RichText::new(ch.model).font(ty.small.clone()).color(c.faint));
                        let (status, status_color) = match state.phase {
                            AiPhase::Editing => ("", c.muted),
                            AiPhase::Streaming => ("generating…", c.amber),
                            AiPhase::Ready => ("ready", c.green),
                        };
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if !status.is_empty() {
                                    ui.label(
                                        RichText::new(status)
                                            .font(ty.small.clone())
                                            .color(status_color),
                                    );
                                }
                            },
                        );
                    });
                    // The header doubles as the title bar. Dragging is confined to it so it
                    // cannot fight the text field or the selection toggle below.
                    let handle = ui.interact(
                        header.response.rect,
                        Id::new("verterm-ai-drag"),
                        egui::Sense::drag(),
                    );
                    if handle.dragged() {
                        drag_delta = handle.drag_delta();
                    }
                    drag_stopped = handle.drag_stopped();
                    let handle = handle.on_hover_cursor(egui::CursorIcon::Grab);
                    if handle.dragged() {
                        ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
                    }
                    let te = TextEdit::singleline(&mut state.input)
                        .font(ty.ui.clone())
                        .desired_width(f32::INFINITY)
                        .hint_text(
                            RichText::new("describe the command you want…")
                                .font(ty.ui.clone())
                                .color(c.faint),
                        )
                        .text_color(c.fg)
                        .frame(chrome::well_frame(c))
                        .lock_focus(true)
                        .show(ui);
                    text_changed = te.response.changed();
                    if state.phase != AiPhase::Streaming {
                        te.response.request_focus();
                    }
                    if let Some(sel) = &state.selection {
                        let label = selection_label(sel);
                        if selection_toggle(ui, state.include_selection, &label, c, ty) {
                            toggle_clicked = true;
                        }
                    }
                    if let Some(err) = &state.error {
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("error: {err}"))
                                    .font(ty.small.clone())
                                    .color(c.red),
                            )
                            .wrap(),
                        );
                        ui.label(
                            RichText::new(format!("endpoint {}", ch.endpoint))
                                .font(ty.small.clone())
                                .color(c.faint),
                        );
                    }
                    if let Some(cmd) = &state.command {
                        chrome::well_frame(c).show(ui, |ui| {
                            ui.set_width(width - 18.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(cmd).font(ty.mono.clone()).color(c.green),
                                )
                                .wrap(),
                            );
                        });
                    } else if state.phase == AiPhase::Streaming && !state.raw.is_empty() {
                        ui.add(
                            egui::Label::new(
                                RichText::new(state.raw.trim())
                                    .font(ty.mono.clone())
                                    .color(c.muted),
                            )
                            .wrap(),
                        );
                    }
                    if !state.explanation.is_empty() {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&state.explanation)
                                    .font(ty.small.clone())
                                    .color(c.muted),
                            )
                            .wrap(),
                        );
                    }
                    let mut footer = match state.phase {
                        AiPhase::Editing => "Enter ask   ·   Esc dismiss".to_string(),
                        AiPhase::Streaming => "Esc cancel".to_string(),
                        AiPhase::Ready => {
                            "Enter insert   ·   Ctrl+Enter run   ·   edit + Enter re-ask   ·   Esc dismiss"
                                .to_string()
                        }
                    };
                    if state.selection.is_some() && state.phase != AiPhase::Streaming {
                        footer.push_str("   ·   Ctrl+S selection");
                    }
                    if let Some(q) = &state.retry
                        && state.phase != AiPhase::Streaming
                    {
                        footer.push_str(&format!("   ·   Ctrl+R retry “{}”", elide_words(q, 40)));
                    }
                    ui.label(RichText::new(footer).font(ty.small.clone()).color(c.faint));
                });
        });

    // Measure the card so the next frame places it from its real size rather than the
    // estimate — this is what keeps a bottom-anchored card pinned while the reply grows.
    let size = area.response.rect.size();
    if state.size != Some(size) {
        state.size = Some(size);
        ctx.request_repaint();
    }
    if drag_delta != vec2(0.0, 0.0) {
        // Re-derive the fraction from the moved corner rather than accumulating pixels, so
        // the card is clamped inside the terminal area on every step of the drag.
        state.place = Place::Frac(card_fraction(term_rect, size, pos + drag_delta));
    }
    if drag_stopped && let Place::Frac(f) = state.place {
        outcome = AiOutcome::Moved(f);
    }

    if toggle_clicked {
        state.include_selection = !state.include_selection;
    }
    if retry_key
        && state.phase != AiPhase::Streaming
        && let Some(q) = state.retry.clone()
    {
        state.input = q.clone();
        return AiOutcome::Submit(q);
    }
    if text_changed && state.phase == AiPhase::Ready && state.input != state.last_sent {
        state.phase = AiPhase::Editing;
    }
    if esc {
        outcome = AiOutcome::Close;
    } else if enter {
        match state.phase {
            AiPhase::Editing => {
                let q = state.input.trim().to_string();
                if !q.is_empty() {
                    outcome = AiOutcome::Submit(q);
                }
            }
            AiPhase::Ready => {
                if let Some(cmd) = state.command.clone() {
                    outcome = if ctrl_enter {
                        AiOutcome::Execute(cmd)
                    } else {
                        AiOutcome::Insert(cmd)
                    };
                }
            }
            AiPhase::Streaming => {}
        }
    }
    outcome
}

// ---------------------------------------------------------------------------------------
// Search bar (vi mode)
// ---------------------------------------------------------------------------------------

pub enum SearchOutcome {
    None,
    Changed,
    Confirm,
    Cancel,
}

#[allow(clippy::too_many_arguments)]
pub fn show_search(
    ctx: &egui::Context,
    term_rect: Rect,
    query: &mut String,
    backwards: bool,
    matched: bool,
    colors: &UiColors,
    ty: &Typography,
    metrics: CellMetrics,
) -> SearchOutcome {
    let (esc, enter) = ctx.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter)));
    let h = metrics.h + 18.0;
    let pos = pos2(term_rect.left(), term_rect.bottom() - h - 6.0);
    let mut changed = false;
    Area::new(Id::new("verterm-search"))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            chrome::card_frame(colors)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .stroke(Stroke::new(
                    1.0,
                    if matched || query.is_empty() {
                        colors.border
                    } else {
                        colors.red
                    },
                ))
                .show(ui, |ui| {
                    ui.set_width(term_rect.width() - 20.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(if backwards { "?" } else { "/" })
                                .font(ty.strong.clone())
                                .color(colors.accent),
                        );
                        let te = TextEdit::singleline(query)
                            .font(ty.mono.clone())
                            .desired_width(f32::INFINITY)
                            .frame(egui::Frame::NONE)
                            .text_color(colors.fg)
                            .lock_focus(true)
                            .show(ui);
                        changed = te.response.changed();
                        te.response.request_focus();
                    });
                });
        });
    if esc {
        SearchOutcome::Cancel
    } else if enter {
        SearchOutcome::Confirm
    } else if changed {
        SearchOutcome::Changed
    } else {
        SearchOutcome::None
    }
}

// ---------------------------------------------------------------------------------------
// Command palette
// ---------------------------------------------------------------------------------------

#[derive(Default)]
pub struct PaletteState {
    pub filter: String,
    pub selected: usize,
}

pub enum PaletteOutcome {
    None,
    Run(Action),
    Close,
}

fn fuzzy_score(needle: &str, hay: &str) -> Option<usize> {
    let hay_l = hay.to_lowercase();
    let needle_l = needle.to_lowercase();
    if needle_l.is_empty() {
        return Some(0);
    }
    if let Some(idx) = hay_l.find(&needle_l) {
        return Some(idx);
    }
    // Subsequence match, scored by span.
    let mut it = hay_l.char_indices();
    let mut first = None;
    let mut last = 0;
    for nc in needle_l.chars() {
        let mut found = false;
        for (i, hc) in it.by_ref() {
            if hc == nc {
                first.get_or_insert(i);
                last = i;
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }
    Some(1000 + last - first.unwrap_or(0))
}

pub fn show_palette(
    ctx: &egui::Context,
    screen: Rect,
    state: &mut PaletteState,
    keymap: &Keymap,
    colors: &UiColors,
    ty: &Typography,
) -> PaletteOutcome {
    let (esc, enter, up, down) = ctx.input(|i| {
        (
            i.key_pressed(Key::Escape),
            i.key_pressed(Key::Enter),
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
        )
    });
    let mut items: Vec<(usize, Action)> = Action::all()
        .into_iter()
        .filter(|a| !matches!(a, Action::SelectTab(_)) || !state.filter.is_empty())
        .filter_map(|a| fuzzy_score(&state.filter, &a.label()).map(|s| (s, a)))
        .collect();
    items.sort_by_key(|(s, a)| (*s, a.label()));
    let items: Vec<Action> = items.into_iter().map(|(_, a)| a).take(14).collect();
    if !items.is_empty() {
        if down {
            state.selected = (state.selected + 1) % items.len();
        }
        if up {
            state.selected = (state.selected + items.len() - 1) % items.len();
        }
        state.selected = state.selected.min(items.len() - 1);
    }

    let width = (screen.width() * 0.55).clamp(380.0, 720.0);
    let row_h = (ty.ui.size * 2.2).round();
    let mut clicked: Option<Action> = None;
    Area::new(Id::new("verterm-palette"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 64.0))
        .show(ctx, |ui| {
            chrome::card_frame(colors).show(ui, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                let te = TextEdit::singleline(&mut state.filter)
                    .font(ty.ui.clone())
                    .desired_width(f32::INFINITY)
                    .hint_text(
                        RichText::new("run a command…")
                            .font(ty.ui.clone())
                            .color(colors.faint),
                    )
                    .text_color(colors.fg)
                    .frame(chrome::well_frame(colors))
                    .lock_focus(true)
                    .show(ui);
                if te.response.changed() {
                    state.selected = 0;
                }
                te.response.request_focus();
                ui.add_space(2.0);
                for (i, action) in items.iter().enumerate() {
                    let selected = i == state.selected;
                    let (rect, resp) = ui.allocate_exact_size(
                        vec2(ui.available_width(), row_h),
                        egui::Sense::click(),
                    );
                    let p = ui.painter();
                    let radius = CornerRadius::same(chrome::R_ROW);
                    if selected {
                        p.rect_filled(rect, radius, colors.accent_soft);
                    } else if resp.hovered() {
                        p.rect_filled(rect, radius, colors.surface_hi);
                    }
                    let mut right = rect.right() - 10.0;
                    if let Some(chord) = keymap.chord_for(*action) {
                        let chip = Chip::solid(
                            p,
                            &chord.display(),
                            &ty.mono_small,
                            colors.muted,
                            colors.surface_alt,
                        );
                        right -= chip.width();
                        chip.paint(p, right, rect.center().y);
                        right -= 8.0;
                    }
                    let (font, color) = if selected {
                        (&ty.strong, colors.fg)
                    } else {
                        (&ty.ui, colors.fg.gamma_multiply(0.82))
                    };
                    chrome::line(
                        p,
                        rect.left() + 12.0,
                        rect.center().y,
                        &action.label(),
                        font,
                        color,
                        (right - rect.left() - 20.0).max(0.0),
                    );
                    if resp.clicked() {
                        clicked = Some(*action);
                    }
                }
                if items.is_empty() {
                    ui.label(
                        RichText::new("no matching command")
                            .font(ty.small.clone())
                            .color(colors.faint),
                    );
                }
            });
        });

    if esc {
        PaletteOutcome::Close
    } else if let Some(a) = clicked {
        PaletteOutcome::Run(a)
    } else if enter {
        items
            .get(state.selected)
            .map(|a| PaletteOutcome::Run(*a))
            .unwrap_or(PaletteOutcome::None)
    } else {
        PaletteOutcome::None
    }
}

// ---------------------------------------------------------------------------------------
// Simple text prompt (scratchpad command)
// ---------------------------------------------------------------------------------------

#[derive(Default)]
pub struct PromptState {
    pub text: String,
}

pub enum PromptOutcome {
    None,
    Submit(String),
    Cancel,
}

pub fn show_prompt(
    ctx: &egui::Context,
    screen: Rect,
    title: &str,
    state: &mut PromptState,
    colors: &UiColors,
    ty: &Typography,
) -> PromptOutcome {
    let (esc, enter) = ctx.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter)));
    let width = (screen.width() * 0.5).clamp(340.0, 640.0);
    Area::new(Id::new("verterm-prompt"))
        .order(Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 64.0))
        .show(ctx, |ui| {
            chrome::card_frame(colors)
                .stroke(Stroke::new(1.0, tint(colors.amber, 90)))
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
                    ui.label(
                        RichText::new(title.to_uppercase())
                            .font(ty.micro.clone())
                            .color(colors.amber),
                    );
                    let te = TextEdit::singleline(&mut state.text)
                        .font(ty.mono.clone())
                        .desired_width(f32::INFINITY)
                        .text_color(colors.fg)
                        .frame(chrome::well_frame(colors))
                        .lock_focus(true)
                        .show(ui);
                    te.response.request_focus();
                    ui.label(
                        RichText::new(
                            "Enter run in a scratchpad tab (closes on exit)   ·   Esc cancel",
                        )
                        .font(ty.small.clone())
                        .color(colors.faint),
                    );
                });
        });
    if esc {
        PromptOutcome::Cancel
    } else if enter {
        let t = state.text.trim().to_string();
        if t.is_empty() {
            PromptOutcome::Cancel
        } else {
            PromptOutcome::Submit(t)
        }
    } else {
        PromptOutcome::None
    }
}

/// Small transient messages in the bottom-right corner.
pub fn show_toasts(
    ctx: &egui::Context,
    screen: Rect,
    toasts: &[(std::time::Instant, String)],
    colors: &UiColors,
    ty: &Typography,
) {
    if toasts.is_empty() {
        return;
    }
    Area::new(Id::new("verterm-toasts"))
        .order(Order::Tooltip)
        .anchor(Align2::RIGHT_BOTTOM, vec2(-14.0, -14.0))
        .interactable(false)
        .show(ctx, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 6.0);
            for (_, msg) in toasts {
                chrome::card_frame(colors)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_max_width(screen.width() * 0.5);
                        let top = ui.cursor().min;
                        ui.add(
                            egui::Label::new(
                                RichText::new(msg).font(ty.small.clone()).color(colors.fg),
                            )
                            .wrap(),
                        );
                        // Accent rule down the left edge of the toast.
                        let h = ui.min_rect().height().max(ty.small.size);
                        ui.painter().rect_filled(
                            Rect::from_min_size(pos2(top.x - 8.0, top.y), vec2(2.0, h)),
                            CornerRadius::same(1),
                            colors.accent,
                        );
                    });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_event(key: Key, pressed: bool, ctrl: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl,
                command: ctrl,
                ..Default::default()
            },
        }
    }

    #[test]
    fn ctrl_chord_is_read_from_the_event_not_the_end_of_frame_modifiers() {
        // Press and release in the same frame: the frame-global modifier state is already
        // back to NONE by the time the overlay looks, but the press event still carries it.
        let events = vec![
            key_event(Key::S, true, true),
            key_event(Key::S, false, true),
        ];
        assert!(ctrl_pressed(&events, Key::S));
        assert!(!ctrl_pressed(&events, Key::Enter));
    }

    #[test]
    fn ctrl_chord_ignores_releases_and_unmodified_presses() {
        assert!(!ctrl_pressed(&[key_event(Key::S, false, true)], Key::S));
        assert!(!ctrl_pressed(&[key_event(Key::S, true, false)], Key::S));
    }

    fn with_selection(sel: Option<&str>) -> AiOverlayState {
        AiOverlayState::new(AiOpen {
            anchor_row: 0,
            selection: sel.map(str::to_string),
            position: AiPosition::BottomRight,
            saved_frac: None,
            retry: None,
        })
    }

    fn area() -> Rect {
        Rect::from_min_size(pos2(100.0, 50.0), vec2(1000.0, 800.0))
    }

    #[test]
    fn corners_land_in_their_corners_with_the_margin_kept() {
        let size = vec2(400.0, 200.0);
        let a = area();
        let tl = place_card(a, size, (0.0, 0.0));
        assert_eq!(tl, pos2(a.left() + CARD_MARGIN, a.top() + CARD_MARGIN));

        let br = place_card(a, size, (1.0, 1.0));
        assert_eq!(br.x + size.x, a.right() - CARD_MARGIN);
        assert_eq!(br.y + size.y, a.bottom() - CARD_MARGIN);

        let c = place_card(a, size, (0.5, 0.5));
        assert!((c.x + size.x / 2.0 - a.center().x).abs() < 0.01);
    }

    #[test]
    fn a_remembered_fraction_restores_proportionally_after_a_resize() {
        // The whole point of storing a fraction: bottom-right is still flush bottom-right in
        // a window half the size, and the card is never pushed outside it.
        let size = vec2(400.0, 200.0);
        let small = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 400.0));
        let br = place_card(small, size, (1.0, 1.0));
        assert_eq!(br.x + size.x, small.right() - CARD_MARGIN);
        assert_eq!(br.y + size.y, small.bottom() - CARD_MARGIN);
        // And a two-thirds position stays two-thirds of the way across.
        let f = (2.0 / 3.0, 0.25);
        let big = place_card(area(), size, f);
        assert_eq!(card_fraction(area(), size, big), f);
    }

    #[test]
    fn a_card_larger_than_the_area_is_clamped_not_pushed_offscreen() {
        let huge = vec2(4000.0, 4000.0);
        let p = place_card(area(), huge, (1.0, 1.0));
        assert_eq!(
            p,
            pos2(area().left() + CARD_MARGIN, area().top() + CARD_MARGIN)
        );
        // The fraction of anything in that state is 0, so a later drag starts from the corner.
        assert_eq!(
            card_fraction(area(), huge, pos2(9999.0, 9999.0)),
            (0.0, 0.0)
        );
    }

    #[test]
    fn a_drag_beyond_an_edge_clamps_instead_of_leaving_the_terminal_area() {
        let size = vec2(400.0, 200.0);
        let a = area();
        assert_eq!(card_fraction(a, size, pos2(-5000.0, -5000.0)), (0.0, 0.0));
        assert_eq!(card_fraction(a, size, pos2(5000.0, 5000.0)), (1.0, 1.0));
        // Round-tripping a clamped drag gives a position inside the area.
        let p = place_card(a, size, card_fraction(a, size, pos2(5000.0, 5000.0)));
        assert!(a.contains_rect(Rect::from_min_size(p, size)));
    }

    #[test]
    fn opening_prefers_a_remembered_drag_over_the_configured_corner() {
        let open = |saved, position| {
            AiOverlayState::new(AiOpen {
                anchor_row: 4,
                selection: None,
                position,
                saved_frac: saved,
                retry: None,
            })
            .place
        };
        assert_eq!(
            open(Some((0.3, 0.9)), AiPosition::BottomRight),
            Place::Frac((0.3, 0.9))
        );
        assert_eq!(open(None, AiPosition::BottomRight), Place::Frac((1.0, 1.0)));
        assert_eq!(open(None, AiPosition::Prompt), Place::Prompt);
        // A drag is remembered even for the prompt-anchored setting, which detaches it.
        assert_eq!(
            open(Some((0.0, 0.0)), AiPosition::Prompt),
            Place::Frac((0.0, 0.0))
        );
    }

    #[test]
    fn selection_label_counts_non_blank_lines_and_chars() {
        assert_eq!(selection_label("one"), "send selection · 1 line, 3 chars");
        // A drag past the end of the output picks up empty rows; they must not be counted.
        assert_eq!(
            selection_label("a\n\n\nb\n   \n"),
            "send selection · 2 lines, 10 chars"
        );
    }

    #[test]
    fn selection_defaults_to_on_when_there_is_one_and_is_absent_otherwise() {
        let with = with_selection(Some("boom"));
        assert!(with.include_selection);
        assert_eq!(with.selection_payload().as_deref(), Some("boom"));

        let without = with_selection(None);
        assert!(!without.include_selection);
        assert_eq!(without.selection_payload(), None);
    }

    #[test]
    fn toggling_the_selection_off_drops_it_from_the_payload() {
        let mut st = with_selection(Some("boom"));
        st.include_selection = false;
        assert_eq!(st.selection_payload(), None);
        // The text itself is kept, so toggling back on does not need a re-selection.
        assert_eq!(st.selection.as_deref(), Some("boom"));
    }
}

//! Colour resolution: config → 269-entry alacritty palette (16 base, 216 cube, 24 grey,
//! named specials) and the small set of UI accents used by the rail, badges and overlays.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

use crate::config;

pub const PALETTE_LEN: usize = 269;

pub struct Palette {
    table: [Rgb; PALETTE_LEN],
    pub selection: Rgb,
}

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

fn dim_rgb(c: Rgb) -> Rgb {
    rgb(
        (c.r as f32 * 0.66) as u8,
        (c.g as f32 * 0.66) as u8,
        (c.b as f32 * 0.66) as u8,
    )
}

impl Palette {
    pub fn from_config(c: &config::Colors) -> Self {
        let def = config::Colors::default();
        let hex = |s: &str, fallback: &str| -> Rgb {
            let (r, g, b) = config::parse_hex(s)
                .or_else(|| config::parse_hex(fallback))
                .unwrap_or((255, 255, 255));
            rgb(r, g, b)
        };
        let fg = hex(&c.foreground, &def.foreground);
        let bg = hex(&c.background, &def.background);
        let cursor = hex(&c.cursor, &def.cursor);

        let mut table = [rgb(0, 0, 0); PALETTE_LEN];
        for i in 0..8 {
            table[i] = hex(
                c.normal.get(i).map(String::as_str).unwrap_or(""),
                &def.normal[i],
            );
            table[i + 8] = hex(
                c.bright.get(i).map(String::as_str).unwrap_or(""),
                &def.bright[i],
            );
        }
        let levels = [0u8, 95, 135, 175, 215, 255];
        let mut idx = 16;
        for r in levels {
            for g in levels {
                for b in levels {
                    table[idx] = rgb(r, g, b);
                    idx += 1;
                }
            }
        }
        for i in 0..24 {
            let v = 8 + 10 * i as u8;
            table[232 + i] = rgb(v, v, v);
        }
        table[NamedColor::Foreground as usize] = fg;
        table[NamedColor::Background as usize] = bg;
        table[NamedColor::Cursor as usize] = cursor;
        for i in 0..8 {
            table[NamedColor::DimBlack as usize + i] = dim_rgb(table[i]);
        }
        table[NamedColor::BrightForeground as usize] = fg;
        table[NamedColor::DimForeground as usize] = dim_rgb(fg);

        Self {
            table,
            selection: hex(&c.selection, &def.selection),
        }
    }

    pub fn rgb_by_index(&self, idx: usize) -> Rgb {
        self.table
            .get(idx)
            .copied()
            .unwrap_or(self.table[NamedColor::Foreground as usize])
    }

    pub fn named(&self, n: NamedColor, overrides: &Colors) -> Rgb {
        overrides[n].unwrap_or(self.table[n as usize])
    }

    /// Resolve a cell colour honouring OSC 4/10/11 overrides stored in the terminal.
    pub fn resolve(&self, color: Color, overrides: &Colors) -> Rgb {
        match color {
            Color::Spec(c) => c,
            Color::Indexed(i) => overrides[i as usize].unwrap_or(self.table[i as usize]),
            Color::Named(n) => overrides[n].unwrap_or(self.table[n as usize]),
        }
    }

    /// Foreground resolution with the classic bold→bright and dim rules.
    pub fn resolve_fg(&self, color: Color, flags: Flags, overrides: &Colors) -> Rgb {
        let dim = flags.contains(Flags::DIM);
        let bold = flags.contains(Flags::BOLD);
        match (color, bold, dim) {
            (Color::Named(n), true, false) if (n as usize) < 8 => {
                self.resolve(Color::Indexed(n as u8 + 8), overrides)
            }
            (Color::Indexed(i), true, false) if i < 8 => {
                self.resolve(Color::Indexed(i + 8), overrides)
            }
            (Color::Named(n), _, true) if (n as usize) < 8 => {
                let dim_named = NamedColor::DimBlack as usize + n as usize;
                overrides[dim_named].unwrap_or(self.table[dim_named])
            }
            (Color::Named(NamedColor::Foreground), _, true) => {
                self.named(NamedColor::DimForeground, overrides)
            }
            (c, _, true) => dim_rgb(self.resolve(c, overrides)),
            (c, _, false) => self.resolve(c, overrides),
        }
    }
}

pub fn to_color32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

/// Chrome design tokens: a surface ladder lifted off the terminal background plus the
/// semantic accents used by the rail, status bar and overlays. Everything is derived from
/// the configured palette so a custom theme stays coherent.
#[derive(Clone, Copy, Debug)]
pub struct UiColors {
    /// The terminal canvas itself — the darkest surface.
    pub bg: Color32,
    /// Window chrome around the canvas (rail, status bar).
    pub surface: Color32,
    /// Cards, wells and input backgrounds sitting on `surface`.
    pub surface_alt: Color32,
    /// Hover / pressed feedback for rows.
    pub surface_hi: Color32,
    /// Floating overlays — the highest surface.
    pub float: Color32,
    /// Hairline separators and card outlines.
    pub border: Color32,
    /// Outline for focused or selected cards.
    pub border_strong: Color32,
    pub fg: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    /// Accent washed into a surface: active rows, chips, selection fills.
    pub accent_soft: Color32,
    pub amber: Color32,
    pub green: Color32,
    pub red: Color32,
    pub vermilion: Color32,
    pub gold: Color32,
    pub blue: Color32,
    pub selection: Color32,
    pub search: Color32,
    pub hint_bg: Color32,
    pub hint_fg: Color32,
}

/// Linear mix of two colours, `t` = 0 keeps `a`.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let c = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).clamp(0.0, 255.0) as u8;
    Color32::from_rgb(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()))
}

/// The same colour at `alpha`, for tinted chips painted over a surface.
pub fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

impl UiColors {
    pub fn from_palette(p: &Palette, cfg: &config::Colors) -> Self {
        let bg = to_color32(p.rgb_by_index(NamedColor::Background as usize));
        let fg = to_color32(p.rgb_by_index(NamedColor::Foreground as usize));
        // Surfaces lift toward a cool white rather than toward the foreground, so a strongly
        // tinted palette does not stain the chrome.
        const LIFT: Color32 = Color32::from_rgb(0xdc, 0xe6, 0xf2);
        let up = |t: f32| mix(bg, LIFT, t);
        let text = |t: f32| mix(bg, fg, t);
        let accent = config::parse_hex(&cfg.accent)
            .map(|(r, g, b)| Color32::from_rgb(r, g, b))
            .unwrap_or(Color32::from_rgb(0x7f, 0xd1, 0xc1));
        Self {
            bg,
            surface: up(0.045),
            surface_alt: up(0.085),
            surface_hi: up(0.13),
            float: up(0.11),
            border: up(0.16),
            border_strong: up(0.30),
            fg,
            muted: text(0.62),
            faint: text(0.38),
            accent,
            accent_soft: tint(accent, 38),
            amber: Color32::from_rgb(0xf5, 0x9e, 0x0b),
            green: Color32::from_rgb(0x4a, 0xde, 0x80),
            red: Color32::from_rgb(0xf8, 0x71, 0x71),
            vermilion: Color32::from_rgb(0xfb, 0x71, 0x85),
            gold: Color32::from_rgb(0xfa, 0xcc, 0x15),
            blue: Color32::from_rgb(0x93, 0xc5, 0xfd),
            selection: to_color32(p.selection),
            search: Color32::from_rgb(0x8a, 0x6d, 0x1f),
            hint_bg: Color32::from_rgb(0xf5, 0x9e, 0x0b),
            hint_fg: Color32::from_rgb(0x10, 0x10, 0x10),
        }
    }
}

pub fn visuals(c: &UiColors) -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = c.surface;
    v.window_fill = c.float;
    v.extreme_bg_color = c.surface_alt;
    v.faint_bg_color = c.surface_hi;
    v.override_text_color = Some(c.fg);
    v.selection.bg_fill = c.accent_soft;
    v.selection.stroke = egui::Stroke::new(1.0, c.accent);
    v.widgets.noninteractive.bg_fill = c.surface;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, c.border);
    v.widgets.inactive.bg_fill = c.surface_alt;
    v.widgets.hovered.bg_fill = c.surface_hi;
    v.widgets.active.bg_fill = c.surface_hi;
    v.window_stroke = egui::Stroke::new(1.0, c.border);
    v.window_corner_radius = egui::CornerRadius::same(super::chrome::R_CARD);
    v.window_shadow = super::chrome::shadow();
    v.popup_shadow = super::chrome::shadow();
    v
}

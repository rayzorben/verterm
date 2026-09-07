//! Colour resolution: a resolved [`Scheme`] → the 269-entry alacritty palette (16 base, 216
//! cube, 24 grey, named specials) and the chrome tokens used by the rail, badges and overlays.
//!
//! Only the sixteen ANSI colours and the four specials are ever configured. Everything else —
//! the surface ladder, the borders, the badge hues, the rail's group tints — is *derived* here,
//! and derived in whichever direction the background points: a light scheme sinks its surfaces
//! toward ink where a dark one lifts them toward white. That is what lets a one-line
//! `theme = "..."` restyle the whole client coherently instead of only the grid.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

use crate::themes::Scheme;

pub const PALETTE_LEN: usize = 269;

pub struct Palette {
    table: [Rgb; PALETTE_LEN],
}

fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

fn to_rgb(c: crate::themes::Rgb) -> Rgb {
    rgb(c[0], c[1], c[2])
}

fn dim_rgb(c: Rgb) -> Rgb {
    rgb(
        (c.r as f32 * 0.66) as u8,
        (c.g as f32 * 0.66) as u8,
        (c.b as f32 * 0.66) as u8,
    )
}

impl Palette {
    pub fn from_scheme(s: &Scheme) -> Self {
        let fg = to_rgb(s.foreground);
        let bg = to_rgb(s.background);
        let cursor = to_rgb(s.cursor);

        let mut table = [rgb(0, 0, 0); PALETTE_LEN];
        for i in 0..8 {
            table[i] = to_rgb(s.normal[i]);
            table[i + 8] = to_rgb(s.bright[i]);
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

        Self { table }
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

/// Chrome design tokens: a surface ladder stepped off the terminal background plus the
/// semantic accents used by the rail, status bar and overlays. Everything is derived from the
/// scheme, in the direction the background points, so a custom or light theme stays coherent.
#[derive(Clone, Copy, Debug)]
pub struct UiColors {
    /// Whether the scheme's background wants light text. Drives which way the surface ladder
    /// steps, which egui `Visuals` base is used, and how heavy the drop shadows are.
    pub dark: bool,
    /// The terminal canvas itself — the plainest surface.
    pub bg: Color32,
    /// Window chrome around the canvas (rail, status bar).
    pub surface: Color32,
    /// Cards, wells and input backgrounds sitting on `surface`.
    pub surface_alt: Color32,
    /// Hover / pressed feedback for rows.
    pub surface_hi: Color32,
    /// Floating overlays — the surface that has to read as *above* the window.
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
    /// The hues a rail group is banded with, in the order they are handed out. Six is enough
    /// that adjacent groups always differ (see `rail::group_tints`) without the rail turning
    /// into a paint chart; they are the scheme's own chromatic ANSI colours, so the banding
    /// belongs to whatever theme is loaded.
    pub group: [Color32; GROUP_TINTS],
}

/// How many distinct group hues the rail rotates through.
pub const GROUP_TINTS: usize = 6;

/// Linear mix of two colours, `t` = 0 keeps `a`.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let c = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).clamp(0.0, 255.0) as u8;
    Color32::from_rgb(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()))
}

/// The same colour at `alpha`, for tinted chips painted over a surface.
pub fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

fn triple(c: Color32) -> crate::themes::Rgb {
    [c.r(), c.g(), c.b()]
}

/// Push `c` away from `bg` until the two clear `min` WCAG contrast, and no further.
///
/// This is what makes a palette's own ANSI colours usable as chrome accents. Half the popular
/// schemes state their red as `#cc241d` or `#d20f39` — fine as terminal text, too dark to be a
/// badge on a light chrome surface and too dark to be a status dot on some dark ones. Rather
/// than shipping a second hand-picked set of chrome colours (which would then not match the
/// theme), the hue is taken from the scheme and only its lightness is corrected.
fn readable(c: Color32, bg: Color32, min: f32) -> Color32 {
    use crate::themes::{contrast, is_dark};
    if contrast(triple(c), triple(bg)) >= min {
        return c;
    }
    let target = if is_dark(triple(bg)) {
        Color32::WHITE
    } else {
        Color32::BLACK
    };
    // Twenty steps is a ~5% lightness quantum: fine enough that the correction is invisible,
    // coarse enough to stay a handful of float ops at startup.
    let mut out = c;
    for i in 1..=20 {
        out = mix(c, target, i as f32 / 20.0);
        if contrast(triple(out), triple(bg)) >= min {
            break;
        }
    }
    out
}

/// Black or white, whichever reads on `bg`. For the one label that is painted *on* an accent.
///
/// Pure black and pure white, not a softened near-black: the worst case here is a mid-luminance
/// badge, where even the better of the two only reaches 4.58 — a hint tag whose whole job is to
/// be read in one glance has no contrast to give away for taste.
fn on(bg: Color32) -> Color32 {
    if crate::themes::is_dark(triple(bg)) {
        Color32::WHITE
    } else {
        Color32::BLACK
    }
}

/// Contrast a chrome accent must clear against the surface it sits on. Below AA body text
/// (4.5) on purpose — these are 12 px chips, dots and 2 px gauges, where AA-large (3.0) is the
/// applicable floor and forcing more would wash every palette toward the same pastel.
const ACCENT_CONTRAST: f32 = 3.2;

impl UiColors {
    pub fn from_scheme(s: &Scheme) -> Self {
        let bg = to_color32(to_rgb(s.background));
        let fg = to_color32(to_rgb(s.foreground));
        let dark = s.dark;

        // Surfaces step *away* from the terminal background — lighter in a dark scheme, darker
        // in a light one — and they step toward a neutral rather than toward the foreground,
        // so a strongly tinted palette does not stain the chrome.
        const LIFT: Color32 = Color32::from_rgb(0xdc, 0xe6, 0xf2);
        const INK: Color32 = Color32::from_rgb(0x1c, 0x22, 0x2b);
        let step = if dark { LIFT } else { INK };
        let up = |t: f32| mix(bg, step, t);
        let text = |t: f32| mix(bg, fg, t);
        let surface = up(0.045);

        // The one rung that does not simply follow the ladder. A floating card has to read as
        // *above* the window, and on a light scheme "above" is lighter, not darker — a dark
        // grey overlay on a white app looks like a modal scrim, not a card.
        let float = if dark {
            up(0.11)
        } else {
            mix(bg, Color32::WHITE, 0.8)
        };

        let hue = |c: Color32| readable(c, surface, ACCENT_CONTRAST);
        let ansi = |i: usize| to_color32(to_rgb(s.normal[i]));
        // Hues come from the *normal* half of the palette in both modes. The bright half is
        // not reliably the same hue — Solarized spends bright 2..5 on greys — so reading the
        // brights would give a grey "success" dot in any scheme that follows that convention.
        let (red, green, yellow) = (ansi(1), ansi(2), ansi(3));
        let (blue, magenta, cyan) = (ansi(4), ansi(5), ansi(6));
        // Two warm accents that must stay apart: `gold` labels an ssh host, `amber` means a
        // command is running. Pulling amber toward red keeps them one glance apart in every
        // palette, which a shared yellow would not.
        let amber = hue(mix(yellow, red, 0.35));
        let gold = hue(yellow);
        // Elevation is not failure, so root must not simply be the failure red; shifting it
        // toward magenta is what has always distinguished the two here.
        let vermilion = hue(mix(red, magenta, 0.35));

        let accent = hue(to_color32(to_rgb(s.accent)));
        let hint_bg = hue(yellow);

        Self {
            dark,
            bg,
            surface,
            surface_alt: up(0.085),
            surface_hi: up(0.13),
            float,
            border: up(0.16),
            border_strong: up(0.30),
            fg,
            muted: text(0.62),
            faint: text(0.38),
            accent,
            accent_soft: tint(accent, if dark { 38 } else { 30 }),
            amber,
            green: hue(green),
            red: hue(red),
            vermilion,
            gold,
            blue: hue(blue),
            selection: to_color32(to_rgb(s.selection)),
            // A wash behind matched text rather than a badge: mixed into the background so the
            // cell's own foreground stays legible on top of it.
            search: mix(bg, yellow, 0.45),
            hint_bg,
            hint_fg: on(hint_bg),
            group: [hue(blue), hue(green), hue(magenta), gold, hue(cyan), amber],
        }
    }
}

pub fn visuals(c: &UiColors) -> egui::Visuals {
    let mut v = if c.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
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
    v.window_shadow = super::chrome::shadow(c.dark);
    v.popup_shadow = super::chrome::shadow(c.dark);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::themes;

    fn scheme(name: &str) -> Scheme {
        themes::find(name)
            .unwrap_or_else(|| panic!("{name} is in the table"))
            .resolve()
    }

    /// The direction the surface ladder steps is the whole light-mode story: get it backwards
    /// and a light theme paints black chrome around a white terminal.
    #[test]
    fn surfaces_step_away_from_the_background_both_ways() {
        let lum = |c: Color32| themes::luminance([c.r(), c.g(), c.b()]);
        for name in ["verterm-dark", "nord", "dracula"] {
            let c = UiColors::from_scheme(&scheme(name));
            assert!(c.dark, "{name} should be dark");
            assert!(lum(c.surface) > lum(c.bg), "{name}: surface must lift");
            assert!(
                lum(c.surface_hi) > lum(c.surface),
                "{name}: hover must lift"
            );
            assert!(
                lum(c.border_strong) > lum(c.border),
                "{name}: border ladder"
            );
        }
        for name in ["catppuccin-latte", "github-light", "gruvbox-light"] {
            let c = UiColors::from_scheme(&scheme(name));
            assert!(!c.dark, "{name} should be light");
            assert!(lum(c.surface) < lum(c.bg), "{name}: surface must sink");
            assert!(
                lum(c.surface_hi) < lum(c.surface),
                "{name}: hover must sink"
            );
            assert!(
                lum(c.border_strong) < lum(c.border),
                "{name}: border ladder"
            );
        }
    }

    /// A floating card reads as *above* the window in both modes, which on a light scheme
    /// means lighter than the page rather than one more rung down the ladder.
    #[test]
    fn a_floating_card_lifts_off_a_light_background_too() {
        let lum = |c: Color32| themes::luminance([c.r(), c.g(), c.b()]);
        for name in ["catppuccin-latte", "one-light", "solarized-light"] {
            let c = UiColors::from_scheme(&scheme(name));
            assert!(lum(c.float) > lum(c.surface), "{name}: overlay must lift");
        }
    }

    /// Every chrome accent has to clear the large-text contrast floor against the surface it
    /// is drawn on, in every built-in. This is the check that the palette-derived hues are
    /// actually usable rather than merely on-theme.
    #[test]
    fn every_accent_is_readable_on_its_own_surface() {
        for t in themes::THEMES {
            let c = UiColors::from_scheme(&t.resolve());
            let bg = [c.surface.r(), c.surface.g(), c.surface.b()];
            let named: [(&str, Color32); 8] = [
                ("accent", c.accent),
                ("amber", c.amber),
                ("green", c.green),
                ("red", c.red),
                ("vermilion", c.vermilion),
                ("gold", c.gold),
                ("blue", c.blue),
                ("fg", c.fg),
            ];
            for (what, col) in named {
                let ratio = themes::contrast([col.r(), col.g(), col.b()], bg);
                assert!(
                    ratio >= ACCENT_CONTRAST - 0.01,
                    "{}: {what} only {ratio:.2} against surface",
                    t.name
                );
            }
            for (i, col) in c.group.iter().enumerate() {
                let ratio = themes::contrast([col.r(), col.g(), col.b()], bg);
                assert!(
                    ratio >= ACCENT_CONTRAST - 0.01,
                    "{}: group tint {i} only {ratio:.2}",
                    t.name
                );
            }
        }
    }

    /// `readable` corrects lightness and stops; a colour that already clears the floor must
    /// come back untouched, or every theme would drift toward pastel.
    #[test]
    fn readable_leaves_a_colour_that_already_clears_the_floor() {
        let bg = Color32::from_rgb(0x10, 0x14, 0x18);
        let ok = Color32::from_rgb(0x7f, 0xd1, 0xc1);
        assert_eq!(readable(ok, bg, 3.0), ok);
    }

    #[test]
    fn readable_lifts_off_a_dark_ground_and_darkens_on_a_light_one() {
        let lum = |c: Color32| themes::luminance([c.r(), c.g(), c.b()]);
        let dim = Color32::from_rgb(0x30, 0x30, 0x30);
        let on_dark = readable(dim, Color32::from_rgb(0x10, 0x10, 0x10), 3.2);
        assert!(lum(on_dark) > lum(dim));
        let pale = Color32::from_rgb(0xee, 0xee, 0xee);
        let on_light = readable(pale, Color32::WHITE, 3.2);
        assert!(lum(on_light) < lum(pale));
    }

    /// The two warm accents mean different things in the rail (`gold` = ssh host, `amber` =
    /// running) and are read side by side, so they must not collapse into one colour.
    #[test]
    fn the_warm_accents_stay_apart_in_every_theme() {
        for t in themes::THEMES {
            let c = UiColors::from_scheme(&t.resolve());
            let d = |a: Color32, b: Color32| {
                (a.r() as i32 - b.r() as i32).abs()
                    + (a.g() as i32 - b.g() as i32).abs()
                    + (a.b() as i32 - b.b() as i32).abs()
            };
            assert!(d(c.amber, c.gold) > 20, "{}: amber == gold", t.name);
            assert!(d(c.vermilion, c.red) > 10, "{}: root == failure", t.name);
        }
    }

    /// The hint label is painted *on* `hint_bg`, so its text colour has to be picked from that
    /// background rather than from the theme's mode.
    #[test]
    fn hint_text_is_readable_on_its_own_badge() {
        for t in themes::THEMES {
            let c = UiColors::from_scheme(&t.resolve());
            let ratio = themes::contrast(
                [c.hint_fg.r(), c.hint_fg.g(), c.hint_fg.b()],
                [c.hint_bg.r(), c.hint_bg.g(), c.hint_bg.b()],
            );
            assert!(ratio >= 4.5, "{}: hint text only {ratio:.2}", t.name);
        }
    }

    /// A dark scheme gets egui's dark widget base, a light one the light base — otherwise the
    /// widgets egui draws for itself (scrollbars, the resize handle) fight the chrome.
    #[test]
    fn visuals_follow_the_scheme() {
        assert!(visuals(&UiColors::from_scheme(&scheme("nord"))).dark_mode);
        assert!(!visuals(&UiColors::from_scheme(&scheme("github-light"))).dark_mode);
    }
}

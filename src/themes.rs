//! Built-in colour schemes, and the resolution of `[colors]` into one.
//!
//! A scheme is the terminal's sixteen ANSI colours plus its specials (foreground, background,
//! cursor, selection) and one chrome accent. Every other pixel verterm draws is *derived* from
//! those — the surface ladder, the borders, the badge hues, the gauges (see
//! `ui::theme::UiColors`) — so naming a scheme in `[colors].theme` restyles the whole client,
//! not just the grid, and a light scheme flips the chrome to a light treatment without any
//! further configuration.
//!
//! Per-key overrides in `[colors]` win over the named scheme, so a theme is a starting point
//! rather than a lock-in.

/// A colour as verterm carries it around before it reaches egui or alacritty.
pub type Rgb = [u8; 3];

/// A built-in scheme, written exactly the way a user would write it in `[colors]`. Kept as hex
/// strings so this table can be read against the upstream project that publishes each palette.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// What the table claims about the background. Never consulted at runtime — the real
    /// answer is measured from the background with [`is_dark`] so an overridden background
    /// flips the chrome too — but asserted against that measurement by a unit test, which is
    /// what keeps a typo in the table from shipping.
    pub dark: bool,
    pub foreground: &'static str,
    pub background: &'static str,
    pub cursor: &'static str,
    pub selection: &'static str,
    /// Chrome accent: active tab, focus rings, overlay headers.
    pub accent: &'static str,
    pub normal: [&'static str; 8],
    pub bright: [&'static str; 8],
}

/// A scheme with every colour parsed and every `[colors]` override applied — what the UI layer
/// actually consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheme {
    /// Measured from `background`, not declared: the chrome asks this to decide whether its
    /// surfaces lift toward white or sink toward ink.
    pub dark: bool,
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub accent: Rgb,
    pub normal: [Rgb; 8],
    pub bright: [Rgb; 8],
}

/// The scheme used when `[colors].theme` is absent or unknown.
pub const DEFAULT: &str = "verterm-dark";

/// Parse `#rrggbb` (or `rrggbb`). Returns `None` on malformed input so callers can fall back.
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// One sRGB channel linearised, for the luminance calculation.
fn linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance, 0 (black) to 1 (white).
pub fn luminance(c: Rgb) -> f32 {
    0.2126 * linear(c[0]) + 0.7152 * linear(c[1]) + 0.0722 * linear(c[2])
}

/// WCAG contrast ratio between two opaque colours, 1.0 (identical) to 21.0 (black on white).
pub fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

/// Whether a background wants light text on it. This is the "which of black or white reads
/// better here" test rather than an eyeballed luminance cutoff, which is the same question the
/// chrome is asking when it decides which way its surfaces should move.
pub fn is_dark(bg: Rgb) -> bool {
    contrast(bg, [255, 255, 255]) > contrast(bg, [0, 0, 0])
}

impl Theme {
    /// Parse the table entry. A malformed hex string in the table would be a bug, so it falls
    /// back to a visible magenta rather than failing at startup; the `every_theme_parses` test
    /// is what actually catches it.
    pub fn resolve(&self) -> Scheme {
        let hex = |s: &str| parse_hex(s).unwrap_or([255, 0, 255]);
        let eight = |src: &[&'static str; 8]| {
            let mut out = [[0u8; 3]; 8];
            for (dst, s) in out.iter_mut().zip(src.iter()) {
                *dst = hex(s);
            }
            out
        };
        let background = hex(self.background);
        Scheme {
            dark: is_dark(background),
            foreground: hex(self.foreground),
            background,
            cursor: hex(self.cursor),
            selection: hex(self.selection),
            accent: hex(self.accent),
            normal: eight(&self.normal),
            bright: eight(&self.bright),
        }
    }
}

/// Look a scheme up by name, tolerating the spellings a user is likely to type: case is
/// ignored and `_` or a space reads as `-`, so `Tokyo Night` and `tokyo_night` both land.
pub fn find(name: &str) -> Option<&'static Theme> {
    let want = normalise(name);
    THEMES.iter().find(|t| normalise(t.name) == want)
}

fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            '_' | ' ' => '-',
            c => c,
        })
        .collect()
}

/// Every built-in, dark ones first — the order `--list-themes` prints.
pub const THEMES: &[Theme] = &[
    // ---------------------------------------------------------------------------- dark ----
    Theme {
        // verterm's own: a base16-derived palette with a cool near-black ground.
        name: "verterm-dark",
        dark: true,
        foreground: "#d8dee9",
        background: "#101418",
        cursor: "#d8dee9",
        selection: "#2f3b4a",
        accent: "#7fd1c1",
        normal: [
            "#181818", "#ac4242", "#90a959", "#f4bf75", "#6a9fb5", "#aa759f", "#75b5aa", "#d0d0d0",
        ],
        bright: [
            "#6b6b6b", "#c55555", "#aac474", "#feca88", "#82b8c8", "#c28cb8", "#93d3c3", "#f5f5f5",
        ],
    },
    Theme {
        name: "catppuccin-mocha",
        dark: true,
        foreground: "#cdd6f4",
        background: "#1e1e2e",
        cursor: "#f5e0dc",
        selection: "#45475a",
        accent: "#cba6f7",
        normal: [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
        ],
        bright: [
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
        ],
    },
    Theme {
        name: "tokyo-night",
        dark: true,
        foreground: "#c0caf5",
        background: "#1a1b26",
        cursor: "#c0caf5",
        selection: "#283457",
        accent: "#7aa2f7",
        normal: [
            "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
        ],
        bright: [
            "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
        ],
    },
    Theme {
        name: "dracula",
        dark: true,
        foreground: "#f8f8f2",
        background: "#282a36",
        cursor: "#f8f8f2",
        selection: "#44475a",
        accent: "#bd93f9",
        normal: [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
        ],
        bright: [
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ],
    },
    Theme {
        name: "nord",
        dark: true,
        foreground: "#d8dee9",
        background: "#2e3440",
        cursor: "#d8dee9",
        selection: "#434c5e",
        accent: "#88c0d0",
        normal: [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
        ],
        bright: [
            "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
        ],
    },
    Theme {
        name: "gruvbox-dark",
        dark: true,
        foreground: "#ebdbb2",
        background: "#282828",
        cursor: "#ebdbb2",
        selection: "#504945",
        accent: "#fabd2f",
        normal: [
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
        ],
        bright: [
            "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
        ],
    },
    Theme {
        name: "one-dark",
        dark: true,
        foreground: "#abb2bf",
        background: "#282c34",
        cursor: "#528bff",
        selection: "#3e4451",
        accent: "#61afef",
        normal: [
            "#282c34", "#e06c75", "#98c379", "#d19a66", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
        ],
        bright: [
            "#5c6370", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#ffffff",
        ],
    },
    Theme {
        name: "solarized-dark",
        dark: true,
        foreground: "#93a1a1",
        background: "#002b36",
        cursor: "#93a1a1",
        selection: "#073642",
        accent: "#2aa198",
        normal: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
        ],
        bright: [
            "#586e75", "#cb4b16", "#859900", "#b58900", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    // --------------------------------------------------------------------------- light ----
    Theme {
        name: "catppuccin-latte",
        dark: false,
        foreground: "#4c4f69",
        background: "#eff1f5",
        cursor: "#dc8a78",
        selection: "#acb0be",
        accent: "#8839ef",
        normal: [
            "#5c5f77", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#acb0be",
        ],
        bright: [
            "#6c6f85", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#bcc0cc",
        ],
    },
    Theme {
        name: "tokyo-night-day",
        dark: false,
        foreground: "#3760bf",
        background: "#e1e2e7",
        cursor: "#3760bf",
        selection: "#b6bfe2",
        accent: "#2e7de9",
        normal: [
            "#e9e9ed", "#f52a65", "#587539", "#8c6c3e", "#2e7de9", "#9854f1", "#007197", "#6172b0",
        ],
        bright: [
            "#a1a6c5", "#f52a65", "#587539", "#8c6c3e", "#2e7de9", "#9854f1", "#007197", "#3760bf",
        ],
    },
    Theme {
        // Solarized's own body text on a light ground is base00 (`#657b83`), which measures
        // 4.13 against base3 — under WCAG AA, and the source of the long-standing "solarized
        // light is washed out" complaint. This uses base01, one rung up the same ramp and
        // Solarized's own "emphasized content" colour, for 5.7.
        name: "solarized-light",
        dark: false,
        foreground: "#586e75",
        background: "#fdf6e3",
        cursor: "#586e75",
        selection: "#eee8d5",
        accent: "#268bd2",
        normal: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
        ],
        bright: [
            "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    Theme {
        name: "gruvbox-light",
        dark: false,
        foreground: "#3c3836",
        background: "#fbf1c7",
        cursor: "#3c3836",
        selection: "#d5c4a1",
        accent: "#b57614",
        normal: [
            "#fbf1c7", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#7c6f64",
        ],
        bright: [
            "#928374", "#9d0006", "#79740e", "#b57614", "#076678", "#8f3f71", "#427b58", "#3c3836",
        ],
    },
    Theme {
        name: "one-light",
        dark: false,
        foreground: "#383a42",
        background: "#fafafa",
        cursor: "#526fff",
        selection: "#d4d4d4",
        accent: "#4078f2",
        normal: [
            "#fafafa", "#ca1243", "#50a14f", "#c18401", "#4078f2", "#a626a4", "#0184bc", "#383a42",
        ],
        bright: [
            "#a0a1a7", "#ca1243", "#50a14f", "#986801", "#4078f2", "#a626a4", "#0184bc", "#090a0b",
        ],
    },
    Theme {
        name: "github-light",
        dark: false,
        foreground: "#24292f",
        background: "#ffffff",
        cursor: "#24292f",
        selection: "#b6d6fd",
        accent: "#0969da",
        normal: [
            "#24292f", "#cf222e", "#116329", "#4d2d00", "#0969da", "#8250df", "#1b7c83", "#6e7781",
        ],
        bright: [
            "#57606a", "#a40e26", "#1a7f37", "#633c01", "#218bff", "#a475f9", "#3192aa", "#8c959f",
        ],
    },
    Theme {
        name: "everforest-light",
        dark: false,
        foreground: "#5c6a72",
        background: "#fffbef",
        cursor: "#5c6a72",
        selection: "#f0f0d6",
        accent: "#8da101",
        normal: [
            "#5c6a72", "#f85552", "#8da101", "#dfa000", "#3a94c5", "#df69ba", "#35a77c", "#e0dcc7",
        ],
        bright: [
            "#829181", "#f85552", "#8da101", "#dfa000", "#3a94c5", "#df69ba", "#35a77c", "#d3c6aa",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex("00ff00"), Some([0, 255, 0]));
        assert_eq!(parse_hex("#fff"), None);
        assert_eq!(parse_hex("#gggggg"), None);
    }

    /// Every colour in the table has to be well-formed hex: `Theme::resolve` papers over a
    /// typo with magenta rather than refusing to start, so this is the check that catches one.
    #[test]
    fn every_theme_parses() {
        for t in THEMES {
            for (what, s) in [
                ("foreground", t.foreground),
                ("background", t.background),
                ("cursor", t.cursor),
                ("selection", t.selection),
                ("accent", t.accent),
            ] {
                assert!(parse_hex(s).is_some(), "{}: bad {what} {s:?}", t.name);
            }
            for (i, s) in t.normal.iter().chain(t.bright.iter()).enumerate() {
                assert!(parse_hex(s).is_some(), "{}: bad ansi[{i}] {s:?}", t.name);
            }
        }
    }

    /// The chrome measures light-vs-dark from the background rather than trusting the table,
    /// so the two must agree — a disagreement means the table is mislabelled.
    #[test]
    fn declared_and_measured_darkness_agree() {
        for t in THEMES {
            assert_eq!(
                t.resolve().dark,
                t.dark,
                "{} claims dark={} but its background measures otherwise",
                t.name,
                t.dark
            );
        }
    }

    #[test]
    fn there_are_seven_of_each() {
        let dark = THEMES.iter().filter(|t| t.dark).count();
        let light = THEMES.iter().filter(|t| !t.dark).count();
        // Seven light, seven dark, plus verterm's own default on the dark side.
        assert_eq!(light, 7, "light themes");
        assert_eq!(dark, 8, "dark themes (7 + the default)");
    }

    #[test]
    fn names_are_unique_and_kebab_case() {
        let mut seen = std::collections::HashSet::new();
        for t in THEMES {
            assert!(seen.insert(t.name), "duplicate theme name {}", t.name);
            assert!(
                t.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{} is not kebab-case",
                t.name
            );
        }
    }

    #[test]
    fn lookup_tolerates_the_spellings_people_type() {
        assert_eq!(find("tokyo-night").map(|t| t.name), Some("tokyo-night"));
        assert_eq!(find("Tokyo Night").map(|t| t.name), Some("tokyo-night"));
        assert_eq!(find("tokyo_night").map(|t| t.name), Some("tokyo-night"));
        assert_eq!(find("  NORD ").map(|t| t.name), Some("nord"));
        assert_eq!(find("no-such-theme").map(|t| t.name), None);
    }

    #[test]
    fn the_default_name_resolves() {
        assert!(find(DEFAULT).is_some(), "{DEFAULT} must be in the table");
    }

    /// Every scheme has to be usable, not merely parseable: text on its own background needs
    /// to clear the WCAG AA body-text ratio, or the whole client is unreadable in it.
    #[test]
    fn foreground_is_readable_on_background() {
        for t in THEMES {
            let s = t.resolve();
            let ratio = contrast(s.foreground, s.background);
            assert!(ratio >= 4.5, "{}: fg/bg contrast only {ratio:.2}", t.name);
        }
    }

    #[test]
    fn luminance_spans_the_range() {
        assert!(luminance([0, 0, 0]) < 0.001);
        assert!(luminance([255, 255, 255]) > 0.999);
        assert!((contrast([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 0.01);
    }

    #[test]
    fn darkness_is_measured_from_the_background() {
        assert!(is_dark([0x10, 0x14, 0x18]));
        assert!(!is_dark([0xff, 0xff, 0xff]));
        assert!(!is_dark([0xfb, 0xf1, 0xc7]));
    }
}

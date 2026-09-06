//! Font discovery through fontconfig (`fc-match`) and registration with egui. Two faces are
//! resolved: the user's Nerd Font (regular/bold/italic/bold-italic) for the grid, placed ahead
//! of egui's built-in monospace fonts so any glyph the primary face lacks still renders, and a
//! proportional family for the chrome (rail, status bar, overlays).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontId};

use crate::config;

#[derive(Clone, Debug)]
pub struct TermFonts {
    pub regular: FontFamily,
    pub bold: FontFamily,
    pub italic: FontFamily,
    pub bold_italic: FontFamily,
    /// Proportional face for chrome text.
    pub ui: FontFamily,
    /// Bold cut of the chrome face (falls back to `ui` when the family has no bold).
    pub ui_bold: FontFamily,
    /// Human description for the status line / logs.
    pub description: String,
}

impl TermFonts {
    /// Chrome text: proportional, optionally the bold cut.
    pub fn ui_id(&self, size: f32, strong: bool) -> FontId {
        let family = if strong { &self.ui_bold } else { &self.ui };
        FontId::new(size, family.clone())
    }

    pub fn id(&self, size: f32, bold: bool, italic: bool) -> FontId {
        let family = match (bold, italic) {
            (false, false) => &self.regular,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (true, true) => &self.bold_italic,
        };
        FontId::new(size, family.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    /// Advance of one cell in points.
    pub w: f32,
    /// Height of one cell in points (row height × configured line height).
    pub h: f32,
    /// Natural glyph row height in points, for vertical centring inside the cell.
    pub text_h: f32,
}

struct FcMatch {
    file: PathBuf,
    family: String,
}

fn fc_match(pattern: &str) -> Option<FcMatch> {
    let out = Command::new("fc-match")
        .args(["-f", "%{file}\t%{family}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim_end().split('\t');
    let file = PathBuf::from(parts.next()?.trim());
    let family = parts.next().unwrap_or("").trim().to_string();
    if file.as_os_str().is_empty() || !file.is_file() {
        return None;
    }
    Some(FcMatch { file, family })
}

fn family_matches(requested: &str, matched: &str) -> bool {
    // Generic aliases always accept whatever fontconfig picked for them.
    if requested.eq_ignore_ascii_case("monospace")
        || requested.eq_ignore_ascii_case("sans-serif")
        || requested.eq_ignore_ascii_case("sans")
    {
        return true;
    }
    matched
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case(requested))
}

fn load_variant(
    requested: &str,
    style: &str,
    exclude: Option<&PathBuf>,
) -> Option<(PathBuf, Vec<u8>)> {
    let m = fc_match(&format!("{requested}:style={style}"))?;
    if !family_matches(requested, &m.family) {
        return None;
    }
    if exclude.is_some_and(|e| *e == m.file) {
        return None;
    }
    let bytes = std::fs::read(&m.file).ok()?;
    Some((m.file, bytes))
}

fn register(
    defs: &mut FontDefinitions,
    name: &str,
    bytes: Vec<u8>,
    chain: &[String],
) -> FontFamily {
    defs.font_data
        .insert(name.to_string(), Arc::new(FontData::from_owned(bytes)));
    let family = FontFamily::Name(name.into());
    let mut list = vec![name.to_string()];
    list.extend_from_slice(chain);
    defs.families.insert(family.clone(), list);
    family
}

/// First entry of `list` that fontconfig resolves to a real Regular face.
fn resolve_first(list: &[String]) -> Option<(String, PathBuf, Vec<u8>)> {
    list.iter()
        .find_map(|fam| load_variant(fam, "Regular", None).map(|(f, b)| (fam.clone(), f, b)))
}

/// Resolve the configured family lists, register the faces with egui and return the families
/// to draw with. Falls back to egui's bundled fonts when fontconfig finds nothing.
pub fn install(ctx: &egui::Context, cfg: &config::Font) -> TermFonts {
    let mut defs = FontDefinitions::default();
    let mono_fallback: Vec<String> = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();
    let prop_fallback: Vec<String> = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    // Chrome face. The "strong" cut is Medium where the family has one — at 12–13 px in a dense
    // rail, Bold is too heavy for the title/subtitle contrast we want.
    let (ui_family, ui_bold_family) = match resolve_first(&cfg.ui_family) {
        Some((fam, file, bytes)) => {
            let ui = register(&mut defs, "verterm-ui", bytes, &prop_fallback);
            let mut chain = vec!["verterm-ui".to_string()];
            chain.extend_from_slice(&prop_fallback);
            let strong = ["Medium", "SemiBold", "Bold"]
                .iter()
                .find_map(|style| load_variant(&fam, style, Some(&file)))
                .map(|(_, b)| register(&mut defs, "verterm-ui-strong", b, &chain))
                .unwrap_or_else(|| ui.clone());
            if let Some(list) = defs.families.get_mut(&FontFamily::Proportional) {
                list.insert(0, "verterm-ui".to_string());
            }
            tracing::info!(font = %fam, "ui font loaded");
            (ui, strong)
        }
        None => {
            tracing::warn!("no configured ui_family resolved via fc-match; using egui's built-in");
            (FontFamily::Proportional, FontFamily::Proportional)
        }
    };

    let Some((fam, regular_file, regular_bytes)) = resolve_first(&cfg.family) else {
        tracing::warn!(
            "no configured font family resolved via fc-match; using egui's built-in monospace"
        );
        ctx.set_fonts(defs);
        return TermFonts {
            regular: FontFamily::Monospace,
            bold: FontFamily::Monospace,
            italic: FontFamily::Monospace,
            bold_italic: FontFamily::Monospace,
            ui: ui_family,
            ui_bold: ui_bold_family,
            description: "egui built-in monospace".into(),
        };
    };

    let regular_family = register(&mut defs, "verterm-regular", regular_bytes, &mono_fallback);
    let mut chain_after_regular = vec!["verterm-regular".to_string()];
    chain_after_regular.extend_from_slice(&mono_fallback);

    let bold_family = load_variant(&fam, "Bold", Some(&regular_file))
        .map(|(_, bytes)| register(&mut defs, "verterm-bold", bytes, &chain_after_regular))
        .unwrap_or_else(|| regular_family.clone());
    let italic_family = load_variant(&fam, "Italic", Some(&regular_file))
        .map(|(_, bytes)| register(&mut defs, "verterm-italic", bytes, &chain_after_regular))
        .unwrap_or_else(|| regular_family.clone());
    let bold_italic_family = load_variant(&fam, "Bold Italic", Some(&regular_file))
        .map(|(_, bytes)| {
            register(
                &mut defs,
                "verterm-bold-italic",
                bytes,
                &chain_after_regular,
            )
        })
        .unwrap_or_else(|| bold_family.clone());

    // Also make egui's generic Monospace family prefer the user font.
    if let Some(list) = defs.families.get_mut(&FontFamily::Monospace) {
        list.insert(0, "verterm-regular".to_string());
    }

    ctx.set_fonts(defs);
    tracing::info!(font = %fam, file = %regular_file.display(), "terminal font loaded");
    TermFonts {
        regular: regular_family,
        bold: bold_family,
        italic: italic_family,
        bold_italic: bold_italic_family,
        ui: ui_family,
        ui_bold: ui_bold_family,
        description: fam,
    }
}

/// Rough metrics for use before egui has laid out any font (first frame).
pub fn estimate(size: f32, line_height: f32) -> CellMetrics {
    let row = size * 1.25;
    CellMetrics {
        w: (size * 0.6).max(1.0),
        h: (row * line_height.max(0.5)).max(1.0),
        text_h: row,
    }
}

pub fn measure(ctx: &egui::Context, fonts: &TermFonts, size: f32, line_height: f32) -> CellMetrics {
    ctx.fonts_mut(|f| {
        let id = FontId::new(size, fonts.regular.clone());
        let w = f.glyph_width(&id, 'M');
        let row = f.row_height(&id);
        CellMetrics {
            w: w.max(1.0),
            h: (row * line_height.max(0.5)).max(1.0),
            text_h: row,
        }
    })
}

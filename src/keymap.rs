//! Chords → actions. Defaults follow the spec; `[keys]` in config.toml overrides per action.
//! Ctrl+Shift+C / Ctrl+Shift+V are intercepted by egui-winit as Copy/Paste events and are
//! therefore handled in the input layer, not here.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use egui::{Key, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Action {
    ToggleRail,
    SelectTab(u8),
    NextTab,
    PrevTab,
    MoveTabNextGroup,
    MoveTabPrevGroup,
    ToggleGroup,
    CollapseGroup,
    ExpandGroup,
    ViMode,
    Search,
    /// Focus the rail's find box: locate a session by name/cwd/host, or by what it printed.
    FindSession,
    Hints,
    AiPrompt,
    NewTab,
    CloseTab,
    NewScratchpad,
    CommandPalette,
    FontIncrease,
    FontDecrease,
    FontReset,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToBottom,
    ClearScrollback,
    Quit,
}

impl Action {
    /// Stable identifier used in config.toml.
    pub fn name(self) -> String {
        match self {
            Action::ToggleRail => "toggle_rail".into(),
            Action::SelectTab(n) => format!("select_tab_{n}"),
            Action::NextTab => "next_tab".into(),
            Action::PrevTab => "prev_tab".into(),
            Action::MoveTabNextGroup => "move_tab_next_group".into(),
            Action::MoveTabPrevGroup => "move_tab_prev_group".into(),
            Action::ToggleGroup => "toggle_group".into(),
            Action::CollapseGroup => "collapse_group".into(),
            Action::ExpandGroup => "expand_group".into(),
            Action::ViMode => "vi_mode".into(),
            Action::Search => "search".into(),
            Action::FindSession => "find_session".into(),
            Action::Hints => "hints".into(),
            Action::AiPrompt => "ai_prompt".into(),
            Action::NewTab => "new_tab".into(),
            Action::CloseTab => "close_tab".into(),
            Action::NewScratchpad => "new_scratchpad".into(),
            Action::CommandPalette => "command_palette".into(),
            Action::FontIncrease => "font_increase".into(),
            Action::FontDecrease => "font_decrease".into(),
            Action::FontReset => "font_reset".into(),
            Action::ScrollPageUp => "scroll_page_up".into(),
            Action::ScrollPageDown => "scroll_page_down".into(),
            Action::ScrollToBottom => "scroll_to_bottom".into(),
            Action::ClearScrollback => "clear_scrollback".into(),
            Action::Quit => "quit".into(),
        }
    }

    pub fn label(self) -> String {
        match self {
            Action::ToggleRail => "Toggle tab rail".into(),
            Action::SelectTab(n) => format!("Select tab {n}"),
            Action::NextTab => "Next tab".into(),
            Action::PrevTab => "Previous tab".into(),
            Action::MoveTabNextGroup => "Move tab to next group".into(),
            Action::MoveTabPrevGroup => "Move tab to previous group".into(),
            Action::ToggleGroup => "Collapse / expand current group".into(),
            Action::CollapseGroup => "Collapse current group".into(),
            Action::ExpandGroup => "Expand current group".into(),
            Action::ViMode => "Vi / scrollback mode".into(),
            Action::Search => "Search scrollback".into(),
            Action::FindSession => "Find session".into(),
            Action::Hints => "Fast jump (hints)".into(),
            Action::AiPrompt => "AI command prompt".into(),
            Action::NewTab => "New tab".into(),
            Action::CloseTab => "Close tab".into(),
            Action::NewScratchpad => "New ephemeral scratchpad".into(),
            Action::CommandPalette => "Command palette".into(),
            Action::FontIncrease => "Increase font size".into(),
            Action::FontDecrease => "Decrease font size".into(),
            Action::FontReset => "Reset font size".into(),
            Action::ScrollPageUp => "Scroll page up".into(),
            Action::ScrollPageDown => "Scroll page down".into(),
            Action::ScrollToBottom => "Scroll to bottom".into(),
            Action::ClearScrollback => "Clear scrollback".into(),
            Action::Quit => "Quit verterm".into(),
        }
    }

    pub fn all() -> Vec<Action> {
        let mut v = vec![
            Action::NewTab,
            Action::CloseTab,
            Action::NewScratchpad,
            Action::NextTab,
            Action::PrevTab,
            Action::ToggleRail,
            Action::ToggleGroup,
            Action::CollapseGroup,
            Action::ExpandGroup,
            Action::MoveTabNextGroup,
            Action::MoveTabPrevGroup,
            Action::ViMode,
            Action::Search,
            Action::FindSession,
            Action::Hints,
            Action::AiPrompt,
            Action::CommandPalette,
            Action::FontIncrease,
            Action::FontDecrease,
            Action::FontReset,
            Action::ScrollPageUp,
            Action::ScrollPageDown,
            Action::ScrollToBottom,
            Action::ClearScrollback,
            Action::Quit,
        ];
        v.extend((1..=9).map(Action::SelectTab));
        v
    }

    pub fn from_name(name: &str) -> Option<Action> {
        let name = name.strip_suffix("_alt").unwrap_or(name);
        if let Some(n) = name.strip_prefix("select_tab_") {
            return n
                .parse::<u8>()
                .ok()
                .filter(|n| (1..=9).contains(n))
                .map(Action::SelectTab);
        }
        Action::all().into_iter().find(|a| a.name() == name)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_: bool,
    pub key: Key,
}

impl Chord {
    pub fn new(key: Key) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            super_: false,
            key,
        }
    }
    pub fn ctrl(mut self) -> Self {
        self.ctrl = true;
        self
    }
    pub fn alt(mut self) -> Self {
        self.alt = true;
        self
    }
    pub fn shift(mut self) -> Self {
        self.shift = true;
        self
    }
    pub fn super_(mut self) -> Self {
        self.super_ = true;
        self
    }

    pub fn from_event(key: Key, m: Modifiers, super_down: bool) -> Self {
        Self {
            ctrl: m.ctrl,
            alt: m.alt,
            shift: m.shift,
            super_: super_down,
            key,
        }
    }

    /// Parse `"Ctrl+Shift+T"`, `"Alt+\\"`, `"Super+K"`, `"Alt+1"`.
    pub fn parse(s: &str) -> Result<Chord> {
        let mut chord: Option<Chord> = None;
        let mut mods = (false, false, false, false);
        let parts: Vec<&str> = s
            .split('+')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        // A literal "+" key appears as an empty token pair; handle "Ctrl++" gracefully.
        let key_token = if s.trim_end().ends_with('+') && parts.len() < s.matches('+').count() {
            Some("Plus")
        } else {
            None
        };
        for part in parts.iter() {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => mods.0 = true,
                "alt" | "meta" | "opt" | "option" => mods.1 = true,
                "shift" => mods.2 = true,
                "super" | "mod4" | "win" | "logo" | "cmd" => mods.3 = true,
                _ => {
                    let key = key_from_token(part)
                        .ok_or_else(|| anyhow!("unknown key {part:?} in {s:?}"))?;
                    chord = Some(Chord::new(key));
                }
            }
        }
        if chord.is_none()
            && let Some(tok) = key_token
        {
            chord = key_from_token(tok).map(Chord::new);
        }
        let mut chord = chord.ok_or_else(|| anyhow!("chord {s:?} has no key"))?;
        chord.ctrl = mods.0;
        chord.alt = mods.1;
        chord.shift = mods.2;
        chord.super_ = mods.3;
        Ok(chord)
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.super_ {
            parts.push("Super".to_string());
        }
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        parts.push(key_display(self.key));
        parts.join("+")
    }
}

fn key_display(key: Key) -> String {
    match key {
        Key::Backslash => "\\".into(),
        Key::OpenBracket => "[".into(),
        Key::CloseBracket => "]".into(),
        Key::Num0 => "0".into(),
        Key::Num1 => "1".into(),
        Key::Num2 => "2".into(),
        Key::Num3 => "3".into(),
        Key::Num4 => "4".into(),
        Key::Num5 => "5".into(),
        Key::Num6 => "6".into(),
        Key::Num7 => "7".into(),
        Key::Num8 => "8".into(),
        Key::Num9 => "9".into(),
        Key::Equals => "=".into(),
        Key::Minus => "-".into(),
        other => other.name().to_string(),
    }
}

fn key_from_token(token: &str) -> Option<Key> {
    let lower = token.to_ascii_lowercase();
    let key = match lower.as_str() {
        "\\" | "backslash" => Key::Backslash,
        "[" | "openbracket" | "bracketleft" => Key::OpenBracket,
        "]" | "closebracket" | "bracketright" => Key::CloseBracket,
        "space" | " " => Key::Space,
        "=" | "equals" | "equal" => Key::Equals,
        "-" | "minus" => Key::Minus,
        "+" | "plus" => Key::Plus,
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "/" | "slash" => Key::Slash,
        "`" | "backtick" | "grave" => Key::Backtick,
        ";" | "semicolon" => Key::Semicolon,
        "," | "comma" => Key::Comma,
        "." | "period" => Key::Period,
        "'" | "quote" => Key::Quote,
        "up" | "arrowup" => Key::ArrowUp,
        "down" | "arrowdown" => Key::ArrowDown,
        "left" | "arrowleft" => Key::ArrowLeft,
        "right" | "arrowright" => Key::ArrowRight,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "home" => Key::Home,
        "end" => Key::End,
        "insert" => Key::Insert,
        "delete" | "del" => Key::Delete,
        "backspace" => Key::Backspace,
        "0" | "num0" => Key::Num0,
        "1" | "num1" => Key::Num1,
        "2" | "num2" => Key::Num2,
        "3" | "num3" => Key::Num3,
        "4" | "num4" => Key::Num4,
        "5" | "num5" => Key::Num5,
        "6" | "num6" => Key::Num6,
        "7" | "num7" => Key::Num7,
        "8" | "num8" => Key::Num8,
        "9" | "num9" => Key::Num9,
        _ => {
            if lower.len() == 1 && lower.as_bytes()[0].is_ascii_alphabetic() {
                return Key::from_name(&lower.to_ascii_uppercase());
            }
            return Key::from_name(token);
        }
    };
    Some(key)
}

#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<(Chord, Action)>,
}

impl Keymap {
    pub fn defaults() -> Self {
        use Action::*;
        let mut b: Vec<(Chord, Action)> = vec![
            (Chord::new(Key::Backslash).alt(), ToggleRail),
            (Chord::new(Key::J).alt(), NextTab),
            (Chord::new(Key::K).alt(), PrevTab),
            (Chord::new(Key::J).alt().shift(), MoveTabNextGroup),
            (Chord::new(Key::K).alt().shift(), MoveTabPrevGroup),
            (Chord::new(Key::G).alt(), ToggleGroup),
            (Chord::new(Key::H).alt(), CollapseGroup),
            (Chord::new(Key::L).alt(), ExpandGroup),
            (Chord::new(Key::OpenBracket).ctrl(), ViMode),
            (Chord::new(Key::F).ctrl().shift(), Search),
            (Chord::new(Key::F).alt(), FindSession),
            (Chord::new(Key::U).alt(), Hints),
            (Chord::new(Key::F).super_(), Hints),
            (Chord::new(Key::Space).ctrl(), AiPrompt),
            (Chord::new(Key::K).super_(), AiPrompt),
            (Chord::new(Key::T).ctrl().shift(), NewTab),
            (Chord::new(Key::W).ctrl().shift(), CloseTab),
            (Chord::new(Key::E).ctrl().shift(), NewScratchpad),
            (Chord::new(Key::P).ctrl().shift(), CommandPalette),
            (Chord::new(Key::Equals).ctrl().shift(), FontIncrease),
            (Chord::new(Key::Plus).ctrl().shift(), FontIncrease),
            (Chord::new(Key::Minus).ctrl().shift(), FontDecrease),
            (Chord::new(Key::Num0).ctrl().shift(), FontReset),
            (Chord::new(Key::PageUp).shift(), ScrollPageUp),
            (Chord::new(Key::PageDown).shift(), ScrollPageDown),
            (Chord::new(Key::End).shift(), ScrollToBottom),
        ];
        let digits = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        for (i, k) in digits.into_iter().enumerate() {
            b.push((Chord::new(k).alt(), SelectTab(i as u8 + 1)));
        }
        Self { bindings: b }
    }

    /// Apply `[keys]` overrides. The first binding for an action is replaced; `*_alt` keys add
    /// a second chord. Returns human-readable warnings for entries that could not be applied.
    pub fn apply_config(&mut self, keys: &BTreeMap<String, String>) -> Vec<String> {
        let mut warnings = Vec::new();
        for (name, chord_text) in keys {
            let Some(action) = Action::from_name(name) else {
                warnings.push(format!("[keys] unknown action {name:?}"));
                continue;
            };
            let chord = match Chord::parse(chord_text) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(format!("[keys] {name}: {e}"));
                    continue;
                }
            };
            // Chords are unique: drop any other action currently on this chord.
            self.bindings.retain(|(c, _)| *c != chord);
            if name.ends_with("_alt") {
                self.bindings.push((chord, action));
            } else if let Some(slot) = self.bindings.iter_mut().find(|(_, a)| *a == action) {
                slot.0 = chord;
            } else {
                self.bindings.push((chord, action));
            }
        }
        warnings
    }

    pub fn lookup(&self, chord: Chord) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(c, _)| *c == chord)
            .map(|(_, a)| *a)
    }

    pub fn chord_for(&self, action: Action) -> Option<Chord> {
        self.bindings
            .iter()
            .find(|(_, a)| *a == action)
            .map(|(c, _)| *c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_session_has_a_default_chord_and_a_stable_name() {
        let km = Keymap::defaults();
        assert_eq!(
            km.lookup(Chord::new(Key::F).alt()),
            Some(Action::FindSession)
        );
        // The `[keys]` identifier the README documents.
        assert_eq!(Action::FindSession.name(), "find_session");
        assert_eq!(Action::from_name("find_session"), Some(Action::FindSession));
        // It must not have stolen the scrollback search's chord.
        assert_eq!(
            km.lookup(Chord::new(Key::F).ctrl().shift()),
            Some(Action::Search)
        );
    }

    #[test]
    fn parse_chords() {
        assert_eq!(
            Chord::parse("Alt+\\").unwrap(),
            Chord::new(Key::Backslash).alt()
        );
        assert_eq!(
            Chord::parse("ctrl+shift+t").unwrap(),
            Chord::new(Key::T).ctrl().shift()
        );
        assert_eq!(
            Chord::parse("Super+K").unwrap(),
            Chord::new(Key::K).super_()
        );
        assert_eq!(Chord::parse("Alt+1").unwrap(), Chord::new(Key::Num1).alt());
        assert_eq!(
            Chord::parse("Ctrl+OpenBracket").unwrap(),
            Chord::new(Key::OpenBracket).ctrl()
        );
        assert!(Chord::parse("Ctrl+").is_err());
        assert!(Chord::parse("Hyper+X").is_err());
    }

    #[test]
    fn overrides_replace_and_add() {
        let mut km = Keymap::defaults();
        let mut cfg = BTreeMap::new();
        cfg.insert("vi_mode".to_string(), "Ctrl+Shift+Space".to_string());
        cfg.insert("hints_alt".to_string(), "Alt+F".to_string());
        cfg.insert("bogus".to_string(), "Alt+Z".to_string());
        let warnings = km.apply_config(&cfg);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            km.lookup(Chord::new(Key::Space).ctrl().shift()),
            Some(Action::ViMode)
        );
        assert_eq!(
            km.lookup(Chord::new(Key::OpenBracket).ctrl()),
            None,
            "old chord removed"
        );
        assert_eq!(km.lookup(Chord::new(Key::F).alt()), Some(Action::Hints));
        assert_eq!(
            km.lookup(Chord::new(Key::U).alt()),
            Some(Action::Hints),
            "default kept"
        );
    }

    #[test]
    fn names_round_trip() {
        for a in Action::all() {
            assert_eq!(Action::from_name(&a.name()), Some(a));
        }
    }
}

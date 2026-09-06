//! Translate egui keyboard events into the byte sequences a terminal expects (xterm
//! conventions, honouring DECCKM application-cursor mode). Printable text arrives via
//! `egui::Event::Text` and is handled by `encode_text`; this module covers everything else.

use alacritty_terminal::term::TermMode;
use egui::{Key, Modifiers};

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_param(m: Modifiers) -> u8 {
    1 + (m.shift as u8) + ((m.alt as u8) << 1) + ((m.ctrl as u8) << 2)
}

fn csi(final_byte: char, param: Option<&str>, m: Modifiers) -> Vec<u8> {
    let mp = modifier_param(m);
    let mut s = String::from("\x1b[");
    match (param, mp) {
        (Some(p), 1) => s.push_str(p),
        (Some(p), mp) => s.push_str(&format!("{p};{mp}")),
        (None, 1) => {}
        (None, mp) => s.push_str(&format!("1;{mp}")),
    }
    s.push(final_byte);
    s.into_bytes()
}

fn tilde(code: &str, m: Modifiers) -> Vec<u8> {
    let mp = modifier_param(m);
    if mp == 1 {
        format!("\x1b[{code}~")
    } else {
        format!("\x1b[{code};{mp}~")
    }
    .into_bytes()
}

fn cursor_key(final_byte: char, m: Modifiers, mode: &TermMode) -> Vec<u8> {
    if modifier_param(m) == 1 && mode.contains(TermMode::APP_CURSOR) {
        format!("\x1bO{final_byte}").into_bytes()
    } else {
        csi(final_byte, None, m)
    }
}

fn with_alt(mut bytes: Vec<u8>, alt: bool) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// Encode a non-text key press. Returns `None` when the key should be ignored here (its
/// text form, if any, arrives separately as `Event::Text`).
pub fn encode_key(key: Key, m: Modifiers, mode: &TermMode) -> Option<Vec<u8>> {
    use Key::*;
    let ctrl = m.ctrl;
    let alt = m.alt;
    let bytes = match key {
        ArrowUp => cursor_key('A', m, mode),
        ArrowDown => cursor_key('B', m, mode),
        ArrowRight => cursor_key('C', m, mode),
        ArrowLeft => cursor_key('D', m, mode),
        Home => cursor_key('H', m, mode),
        End => cursor_key('F', m, mode),
        Insert => tilde("2", m),
        Delete => tilde("3", m),
        PageUp => tilde("5", m),
        PageDown => tilde("6", m),
        F1 | F2 | F3 | F4 => {
            let c = match key {
                F1 => 'P',
                F2 => 'Q',
                F3 => 'R',
                _ => 'S',
            };
            if modifier_param(m) == 1 {
                format!("\x1bO{c}").into_bytes()
            } else {
                csi(c, None, m)
            }
        }
        F5 => tilde("15", m),
        F6 => tilde("17", m),
        F7 => tilde("18", m),
        F8 => tilde("19", m),
        F9 => tilde("20", m),
        F10 => tilde("21", m),
        F11 => tilde("23", m),
        F12 => tilde("24", m),
        Enter => with_alt(vec![b'\r'], alt),
        Tab => {
            if m.shift {
                b"\x1b[Z".to_vec()
            } else {
                with_alt(vec![b'\t'], alt)
            }
        }
        Backspace => {
            let b = if ctrl { 0x08 } else { 0x7f };
            with_alt(vec![b], alt)
        }
        Escape => with_alt(vec![0x1b], alt),
        Space if ctrl => with_alt(vec![0x00], alt),
        // Letters only matter here with Ctrl; plain/shift/alt letters arrive as text.
        A | B | C | D | E | F | G | H | I | J | K | L | M | N | O | P | Q | R | S | T | U | V
        | W | X | Y | Z
            if ctrl =>
        {
            let idx = key.name().chars().next()?.to_ascii_lowercase() as u8 - b'a' + 1;
            with_alt(vec![idx], alt)
        }
        Num2 if ctrl => with_alt(vec![0x00], alt),
        Num3 if ctrl => with_alt(vec![0x1b], alt),
        Num4 if ctrl => with_alt(vec![0x1c], alt),
        Num5 if ctrl => with_alt(vec![0x1d], alt),
        Num6 if ctrl => with_alt(vec![0x1e], alt),
        Num7 if ctrl => with_alt(vec![0x1f], alt),
        Num8 if ctrl => with_alt(vec![0x7f], alt),
        OpenBracket if ctrl => with_alt(vec![0x1b], alt),
        Backslash if ctrl => with_alt(vec![0x1c], alt),
        CloseBracket if ctrl => with_alt(vec![0x1d], alt),
        Slash | Minus if ctrl => with_alt(vec![0x1f], alt),
        Backtick if ctrl => with_alt(vec![0x00], alt),
        Questionmark if ctrl => with_alt(vec![0x7f], alt),
        _ => return None,
    };
    Some(bytes)
}

/// Encode committed text; with Alt held each character is ESC-prefixed (meta).
pub fn encode_text(text: &str, alt: bool) -> Vec<u8> {
    if !alt {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        out.push(0x1b);
        let mut buf = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }
    out
}

/// Wheel scrolling inside a full-screen application (alternate screen + alternate scroll
/// mode) is delivered as arrow keys, like every other terminal.
pub fn wheel_as_arrows(lines: i32, mode: &TermMode) -> Vec<u8> {
    let key = if lines > 0 { 'A' } else { 'B' };
    let seq = if mode.contains(TermMode::APP_CURSOR) {
        format!("\x1bO{key}")
    } else {
        format!("\x1b[{key}")
    };
    seq.repeat(lines.unsigned_abs() as usize).into_bytes()
}

pub fn focus_event(focused: bool) -> Vec<u8> {
    if focused {
        b"\x1b[I".to_vec()
    } else {
        b"\x1b[O".to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers::NONE;
    const CTRL: Modifiers = Modifiers::CTRL;

    #[test]
    fn arrows_and_app_cursor() {
        let normal = TermMode::empty();
        let app = TermMode::APP_CURSOR;
        assert_eq!(encode_key(Key::ArrowUp, NONE, &normal).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(Key::ArrowUp, NONE, &app).unwrap(), b"\x1bOA");
        assert_eq!(encode_key(Key::ArrowUp, CTRL, &app).unwrap(), b"\x1b[1;5A");
        assert_eq!(
            encode_key(Key::ArrowLeft, Modifiers::SHIFT | Modifiers::ALT, &normal).unwrap(),
            b"\x1b[1;4D"
        );
    }

    #[test]
    fn editing_keys() {
        let m = TermMode::empty();
        assert_eq!(encode_key(Key::Delete, NONE, &m).unwrap(), b"\x1b[3~");
        assert_eq!(encode_key(Key::PageUp, CTRL, &m).unwrap(), b"\x1b[5;5~");
        assert_eq!(
            encode_key(Key::Tab, Modifiers::SHIFT, &m).unwrap(),
            b"\x1b[Z"
        );
        assert_eq!(encode_key(Key::Backspace, NONE, &m).unwrap(), b"\x7f");
        assert_eq!(encode_key(Key::Backspace, CTRL, &m).unwrap(), b"\x08");
        assert_eq!(encode_key(Key::F5, NONE, &m).unwrap(), b"\x1b[15~");
        assert_eq!(encode_key(Key::F1, NONE, &m).unwrap(), b"\x1bOP");
    }

    #[test]
    fn control_letters_and_meta() {
        let m = TermMode::empty();
        assert_eq!(encode_key(Key::C, CTRL, &m).unwrap(), vec![0x03]);
        assert_eq!(
            encode_key(Key::A, CTRL | Modifiers::ALT, &m).unwrap(),
            vec![0x1b, 0x01]
        );
        assert_eq!(
            encode_key(Key::A, NONE, &m),
            None,
            "plain letters come from Event::Text"
        );
        assert_eq!(encode_key(Key::OpenBracket, CTRL, &m).unwrap(), vec![0x1b]);
        assert_eq!(encode_text("x", true), vec![0x1b, b'x']);
        assert_eq!(encode_text("é", false), "é".as_bytes());
    }
}

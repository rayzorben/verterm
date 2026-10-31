//! BBS mode: the byte-level differences between a DOS-era ANSI-BBS stream and what the
//! xterm-flavoured parser in `alacritty_terminal` expects.
//!
//! Two of them break a BBS screen, and both are fixed here, before the bytes reach the parser:
//!
//! * **Encoding.** BBS software sends code page 437. vte decodes UTF-8, so CP437 box drawing
//!   comes out as U+FFFD, or worse as the wrong letter when two CP437 bytes happen to form a
//!   valid UTF-8 pair (`─┐` = `C4 BF` reads as `Ŀ`). [`BbsFilter`] transcodes every byte
//!   ≥ 0x80 to its CP437 character; bytes below 0x80 are ASCII in both and stay as they are,
//!   so C0 controls and escape sequences are untouched.
//! * **Clear screen.** In ANSI.SYS, `ESC [ 2 J` clears the screen *and homes the cursor*; in
//!   ECMA-48/xterm it leaves the cursor where it was. A BBS clears, then draws its frame with
//!   plain text and fills fields in with absolute positioning, so without the home the frame
//!   lands wherever the cursor was and the fields land at the top. The filter appends
//!   `ESC [ H` after every ED 2.
//!
//! The mode is per tab: on automatically while a telnet client is the PTY's foreground job,
//! and toggled by hand for anything else (telnet inside ssh, a `nc` session). Keystrokes go
//! the other way through [`encode_cp437`].

/// CP437 characters for bytes 0x80..=0xFF.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// `comm` values of telnet clients; their being the foreground job turns BBS mode on.
const TELNET_CLIENTS: &[&str] = &["telnet", "telnet-ssl", "telnet.netkit"];

pub fn is_telnet_comm(comm: &str) -> bool {
    TELNET_CLIENTS.contains(&comm.trim_end())
}

/// Where the filter is inside an escape sequence. Only CSI is tracked, because ED is the one
/// sequence it rewrites; everything else passes through as it arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    /// Inside `ESC [`; `params_are_2` holds while the parameter bytes so far are exactly "2".
    Csi {
        empty: bool,
        params_are_2: bool,
    },
}

/// Streaming CP437 → UTF-8 transcoder that also homes the cursor after ED 2. Sequences split
/// across reads are handled: the state survives between calls.
#[derive(Default)]
pub struct BbsFilter {
    state: State,
}

impl BbsFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget any half-seen sequence (the mode was switched off mid-stream).
    pub fn reset(&mut self) {
        self.state = State::Ground;
    }

    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>) {
        for &b in input {
            if b >= 0x80 {
                // A CP437 character is printable, so it cannot be part of a CSI sequence.
                self.state = State::Ground;
                let mut utf8 = [0u8; 4];
                out.extend_from_slice(
                    CP437_HIGH[(b - 0x80) as usize]
                        .encode_utf8(&mut utf8)
                        .as_bytes(),
                );
                continue;
            }
            out.push(b);
            self.state = match (self.state, b) {
                (_, 0x1b) => State::Esc,
                (State::Esc, b'[') => State::Csi {
                    empty: true,
                    params_are_2: false,
                },
                (State::Esc, _) => State::Ground,
                // C0 controls execute inside a CSI without ending it (ECMA-48 § 5.5).
                (s @ State::Csi { .. }, 0x00..=0x1f) => s,
                (State::Csi { empty, .. }, 0x30..=0x3f) => State::Csi {
                    empty: false,
                    params_are_2: empty && b == b'2',
                },
                (State::Csi { .. }, 0x20..=0x2f) => State::Csi {
                    empty: false,
                    params_are_2: false,
                },
                (State::Csi { params_are_2, .. }, 0x40..=0x7e) => {
                    if params_are_2 && b == b'J' {
                        out.extend_from_slice(b"\x1b[H");
                    }
                    State::Ground
                }
                (State::Csi { .. }, _) => State::Ground,
                (State::Ground, _) => State::Ground,
            };
        }
    }
}

/// Keystrokes and pastes in BBS mode: UTF-8 text → CP437 bytes. Characters CP437 cannot
/// represent become `?`. Input that is not UTF-8 is passed through unchanged.
pub fn encode_cp437(bytes: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return bytes.to_vec();
    };
    if text.is_ascii() {
        return bytes.to_vec();
    }
    text.chars()
        .map(|c| {
            if c.is_ascii() {
                c as u8
            } else {
                CP437_HIGH
                    .iter()
                    .position(|&h| h == c)
                    .map_or(b'?', |i| 0x80 + i as u8)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> String {
        let mut f = BbsFilter::new();
        let mut out = Vec::new();
        for c in chunks {
            f.process(c, &mut out);
        }
        String::from_utf8(out).expect("filter output is UTF-8")
    }

    #[test]
    fn transcodes_box_drawing() {
        // The pairs that read as `Ŀ` and `ͼ` when decoded as UTF-8.
        assert_eq!(run(&[b"\xc4\xbf \xcd\xbc"]), "─┐ ═╝");
        assert_eq!(run(&[b"\xc9\xcd\xbb\r\n\xba \xba"]), "╔═╗\r\n║ ║");
        assert_eq!(run(&[b"\x80\xff"]), "Ç\u{a0}");
    }

    #[test]
    fn ascii_and_sequences_pass_through() {
        let s = "\x1b[1;33mhi\x1b[0m\x1b]0;title\x07\r\n";
        assert_eq!(run(&[s.as_bytes()]), s);
    }

    #[test]
    fn clear_screen_homes_cursor() {
        assert_eq!(run(&[b"a\x1b[2Jb"]), "a\x1b[2J\x1b[Hb");
        // Split anywhere inside the sequence.
        assert_eq!(run(&[b"\x1b", b"[", b"2", b"J"]), "\x1b[2J\x1b[H");
    }

    #[test]
    fn other_erases_are_left_alone() {
        for s in [
            "\x1b[J", "\x1b[0J", "\x1b[1J", "\x1b[22J", "\x1b[12J", "\x1b[?2J", "\x1b[2K",
        ] {
            assert_eq!(run(&[s.as_bytes()]), s, "{s:?}");
        }
    }

    #[test]
    fn high_byte_aborts_a_sequence() {
        // `ESC [ 2` then a CP437 glyph: not an ED, so the following `J` is plain text.
        assert_eq!(run(&[b"\x1b[2\xdbJ"]), "\x1b[2█J");
    }

    #[test]
    fn telnet_clients() {
        assert!(is_telnet_comm("telnet\n"));
        assert!(is_telnet_comm("telnet-ssl"));
        assert!(!is_telnet_comm("ssh"));
        assert!(!is_telnet_comm("telnetd"));
    }

    #[test]
    fn encodes_keystrokes() {
        assert_eq!(encode_cp437(b"look\r"), b"look\r");
        assert_eq!(encode_cp437("é█€".as_bytes()), b"\x82\xdb?");
        assert_eq!(encode_cp437(b"\xff\xfe"), b"\xff\xfe");
    }

    #[test]
    fn round_trip() {
        let all: Vec<u8> = (0x80..=0xff).collect();
        let mut out = Vec::new();
        BbsFilter::new().process(&all, &mut out);
        assert_eq!(encode_cp437(&out), all);
    }
}

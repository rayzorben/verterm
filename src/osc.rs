//! A streaming tap that runs *before* the vte parser and extracts the two OSC families
//! alacritty_terminal ignores: OSC 7 (working directory) and OSC 133 (semantic prompt
//! boundaries: A prompt start, B prompt end, C command start, D;<code> command end).
//!
//! The tap is byte-exact across chunk boundaries and passes everything else through
//! untouched. Sequences it claims are removed from the stream so the terminal never has to
//! see them; every other escape (including other OSCs) is forwarded verbatim.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 7 — `file://host/path`. `host` may be empty.
    Cwd { host: String, path: String },
    /// OSC 133;A
    PromptStart,
    /// OSC 133;B
    PromptEnd,
    /// OSC 133;C — the command line the shell is about to run, when the integration sends
    /// one (`133;C;<cmd>`). `None` for a bare `133;C`.
    CommandStart(Option<String>),
    /// OSC 133;D[;code]
    CommandEnd(Option<i32>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw ESC, waiting to learn if this is `ESC ]`.
    Esc,
    /// Collecting an OSC body that may still be one of ours.
    Osc,
    /// Inside an OSC body, saw ESC — could be the ST terminator `ESC \`.
    OscEsc,
}

/// An event plus the byte offset **in the filtered output** at which its sequence ended.
///
/// The offset is what lets the reader thread advance the terminal parser to exactly that
/// point before sampling the grid: a single PTY read routinely carries `133;C`, the command's
/// output and `133;D` together, and a position sampled after the whole chunk describes none
/// of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscMark {
    pub at: usize,
    pub event: OscEvent,
}

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const MAX_OSC_LEN: usize = 8192;

#[derive(Debug)]
pub struct OscTap {
    state: State,
    buf: Vec<u8>,
}

impl Default for OscTap {
    fn default() -> Self {
        Self::new()
    }
}

impl OscTap {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::with_capacity(256),
        }
    }

    /// Feed `input`; bytes destined for the terminal parser are appended to `out`,
    /// recognised sequences are appended to `events`.
    pub fn process(&mut self, input: &[u8], out: &mut Vec<u8>, events: &mut Vec<OscMark>) {
        for &b in input {
            match self.state {
                State::Ground => {
                    if b == ESC {
                        self.state = State::Esc;
                    } else {
                        out.push(b);
                    }
                }
                State::Esc => {
                    if b == b']' {
                        self.state = State::Osc;
                        self.buf.clear();
                    } else if b == ESC {
                        // Two escapes in a row: emit the first, keep waiting on the second.
                        out.push(ESC);
                    } else {
                        out.push(ESC);
                        out.push(b);
                        self.state = State::Ground;
                    }
                }
                State::Osc => {
                    if b == BEL {
                        self.terminate(out, events, &[BEL]);
                    } else if b == ESC {
                        self.state = State::OscEsc;
                    } else {
                        self.buf.push(b);
                        if !Self::could_be_ours(&self.buf) || self.buf.len() > MAX_OSC_LEN {
                            // Not ours: hand the prefix back to the parser and stop tracking.
                            out.push(ESC);
                            out.push(b']');
                            out.extend_from_slice(&self.buf);
                            self.buf.clear();
                            self.state = State::Ground;
                        }
                    }
                }
                State::OscEsc => {
                    if b == b'\\' {
                        self.terminate(out, events, &[ESC, b'\\']);
                    } else {
                        // Not a string terminator; flush everything we held back.
                        out.push(ESC);
                        out.push(b']');
                        out.extend_from_slice(&self.buf);
                        out.push(ESC);
                        self.buf.clear();
                        if b == ESC {
                            self.state = State::Esc;
                        } else {
                            out.push(b);
                            self.state = State::Ground;
                        }
                    }
                }
            }
        }
    }

    /// While shorter than a prefix, the buffer must be a prefix of it; once longer, it must
    /// start with it. Handles "7;" and "133;".
    fn could_be_ours(buf: &[u8]) -> bool {
        const PREFIXES: [&[u8]; 2] = [b"7;", b"133;"];
        PREFIXES.iter().any(|p| {
            if buf.len() <= p.len() {
                p.starts_with(buf)
            } else {
                buf.starts_with(p)
            }
        })
    }

    fn terminate(&mut self, out: &mut Vec<u8>, events: &mut Vec<OscMark>, terminator: &[u8]) {
        let body = std::mem::take(&mut self.buf);
        self.state = State::Ground;
        match parse_osc(&body) {
            Parsed::Event(event) => events.push(OscMark {
                at: out.len(),
                event,
            }),
            Parsed::Swallow => {}
            Parsed::Forward => {
                out.push(ESC);
                out.push(b']');
                out.extend_from_slice(&body);
                out.extend_from_slice(terminator);
            }
        }
    }
}

enum Parsed {
    Event(OscEvent),
    /// One of our families but a sub-command the terminal has no use for.
    Swallow,
    /// Not ours; hand it to the parser unchanged.
    Forward,
}

fn parse_osc(body: &[u8]) -> Parsed {
    let Ok(text) = std::str::from_utf8(body) else {
        return Parsed::Forward;
    };
    if let Some(rest) = text.strip_prefix("7;") {
        return Parsed::Event(parse_osc7(rest));
    }
    if let Some(rest) = text.strip_prefix("133;") {
        return match parse_osc133(rest) {
            Some(ev) => Parsed::Event(ev),
            None => Parsed::Swallow,
        };
    }
    Parsed::Forward
}

fn parse_osc7(uri: &str) -> OscEvent {
    // Accept `file://host/path`, `file:///path`, or a bare `/path`.
    let (host, path) = match uri.strip_prefix("file://") {
        Some(rest) => match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        },
        None => ("", uri),
    };
    OscEvent::Cwd {
        host: host.to_string(),
        path: percent_decode(path),
    }
}

fn parse_osc133(rest: &str) -> Option<OscEvent> {
    let mut parts = rest.split(';');
    let kind = parts.next()?;
    match kind {
        "A" => Some(OscEvent::PromptStart),
        "B" => Some(OscEvent::PromptEnd),
        // `133;C;<command>`: the command text may itself contain `;`, so it is everything
        // after the first separator, not the next `split` field.
        "C" => Some(OscEvent::CommandStart(
            rest.split_once(';')
                .map(|(_, cmd)| sanitize_command(cmd))
                .filter(|c| !c.is_empty()),
        )),
        "D" => {
            let code = parts.next().and_then(|c| c.trim().parse::<i32>().ok());
            Some(OscEvent::CommandEnd(code))
        }
        // Unknown 133 sub-command (P, N, ...): swallowed, the terminal cannot use it.
        _ => None,
    }
}

/// Collapse control characters and surrounding whitespace out of a reported command line.
/// The shells already do this, but a command typed with a literal control character (or a
/// shell without the hook) must not be able to smuggle one into the AI request or the UI.
fn sanitize_command(cmd: &str) -> String {
    cmd.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Decode `%XX` escapes; malformed escapes are kept literally.
pub fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let decoded = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = decoded {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_marks(chunks: &[&[u8]]) -> (Vec<u8>, Vec<OscMark>) {
        let mut tap = OscTap::new();
        let mut out = Vec::new();
        let mut events = Vec::new();
        for c in chunks {
            tap.process(c, &mut out, &mut events);
        }
        (out, events)
    }

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<OscEvent>) {
        let (out, marks) = run_marks(chunks);
        (out, marks.into_iter().map(|m| m.event).collect())
    }

    #[test]
    fn plain_text_passes_through() {
        let (out, ev) = run(&[b"hello \x1b[31mred\x1b[0m\n"]);
        assert_eq!(out, b"hello \x1b[31mred\x1b[0m\n");
        assert!(ev.is_empty());
    }

    #[test]
    fn osc7_is_extracted_and_removed() {
        let (out, ev) = run(&[b"a\x1b]7;file://box/home/me/src%20x\x07b"]);
        assert_eq!(out, b"ab");
        assert_eq!(
            ev,
            vec![OscEvent::Cwd {
                host: "box".into(),
                path: "/home/me/src x".into()
            }]
        );
    }

    #[test]
    fn osc133_variants_with_st_terminator() {
        let (out, ev) = run(&[b"\x1b]133;A\x1b\\\x1b]133;C\x1b\\\x1b]133;D;127\x1b\\x"]);
        assert_eq!(out, b"x");
        assert_eq!(
            ev,
            vec![
                OscEvent::PromptStart,
                OscEvent::CommandStart(None),
                OscEvent::CommandEnd(Some(127))
            ]
        );
    }

    #[test]
    fn split_across_chunks() {
        let (out, ev) = run(&[b"pre\x1b", b"]13", b"3;D;", b"1\x07post"]);
        assert_eq!(out, b"prepost");
        assert_eq!(ev, vec![OscEvent::CommandEnd(Some(1))]);
    }

    #[test]
    fn other_osc_forwarded_verbatim() {
        let seq = b"\x1b]0;title here\x07\x1b]52;c;aGk=\x1b\\";
        let (out, ev) = run(&[seq]);
        assert_eq!(out, seq.to_vec());
        assert!(ev.is_empty());
    }

    #[test]
    fn osc_prefix_lookalikes_forwarded() {
        // OSC 70 and OSC 13 share leading digits with 7; and 133; but are not ours.
        let seq = b"\x1b]70;x\x07\x1b]13;y\x07\x1b]1337;z\x07";
        let (out, ev) = run(&[seq]);
        assert_eq!(out, seq.to_vec());
        assert!(ev.is_empty());
    }

    #[test]
    fn esc_inside_osc_that_is_not_st() {
        let seq = b"\x1b]7;file:///tmp\x1b[0m";
        let (out, ev) = run(&[seq]);
        assert_eq!(out, seq.to_vec());
        assert!(ev.is_empty());
    }

    #[test]
    fn double_escape_kept() {
        let (out, _) = run(&[b"\x1b\x1b[A"]);
        assert_eq!(out, b"\x1b\x1b[A");
    }
    #[test]
    fn osc133_c_carries_the_command_line_including_semicolons() {
        let (out, ev) = run(&[b"\x1b]133;C;git log --oneline; false\x07x"]);
        assert_eq!(out, b"x");
        assert_eq!(
            ev,
            vec![OscEvent::CommandStart(Some(
                "git log --oneline; false".into()
            ))]
        );
    }

    #[test]
    fn osc133_c_strips_control_characters_and_empty_commands() {
        let (_, ev) = run(&[b"\x1b]133;C;ls\t-la\x07"]);
        assert_eq!(ev, vec![OscEvent::CommandStart(Some("ls -la".into()))]);
        // A shell that sends the separator but no command must not report an empty string.
        let (_, ev) = run(&[b"\x1b]133;C;   \x07"]);
        assert_eq!(ev, vec![OscEvent::CommandStart(None)]);
    }

    #[test]
    fn marks_carry_the_offset_of_the_sequence_in_the_filtered_output() {
        // One chunk with output on both sides of the marks: the offsets are what let the
        // reader advance the parser to each boundary before sampling the grid.
        let (out, marks) = run_marks(&[b"a\x1b]133;C;ls\x07bbbb\x1b]133;D;1\x07cc\x1b]133;A\x07"]);
        assert_eq!(out, b"abbbbcc");
        let offsets: Vec<usize> = marks.iter().map(|m| m.at).collect();
        assert_eq!(offsets, vec![1, 5, 7]);
    }

    #[test]
    fn mark_offsets_are_relative_to_the_output_buffer_across_chunks() {
        let (out, marks) = run_marks(&[b"xy", b"\x1b]133;A\x07", b"zz\x1b]133;C\x07"]);
        assert_eq!(out, b"xyzz");
        assert_eq!(marks.iter().map(|m| m.at).collect::<Vec<_>>(), vec![2, 4]);
    }
}

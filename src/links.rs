//! Hyperlink recognition: turning terminal text into URIs something can be asked to open.
//!
//! Two sources feed the same [`GridLink`] downstream (see `ui::terminal_view`):
//!
//! * **OSC 8** — the explicit escape sequence (`ESC ] 8 ; params ; URI ST`). `alacritty_terminal`
//!   parses it for us and hangs the URI off every cell it covers, so those links need no guessing
//!   at all. They are authoritative and always win over detection.
//! * **Detection** — this module, scanning rendered rows for things that *look* like URIs.
//!
//! Detection follows the grammar rather than a convenient approximation:
//!
//! * A scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` (RFC 3986 §3.1) and must appear in
//!   an **allowlist**. This is the security boundary, not a nicety: any byte stream a program
//!   prints could otherwise hand an arbitrary URI handler a click, and `xdg-open` will happily
//!   dispatch schemes nobody meant to expose. A scheme not in the list is not a link.
//! * The body charset is exactly what RFC 3986 permits unencoded — unreserved, gen-delims,
//!   sub-delims and `%` — widened to non-ASCII for IRIs (RFC 3987) minus the Unicode spaces and
//!   General Punctuation block, where the typographic quotes and dashes that wrap URLs in prose
//!   live. Space, `"`, `<`, `>`, `` ` ``, `|`, `\`, `^`, `{`, `}` are excluded by the RFC itself,
//!   which is why `<https://x/>` and `"https://x"` stop in the right place for free.
//! * A hierarchical scheme must be followed by `//` and a non-empty authority. `file: not found`
//!   and `git:reflog` are therefore not links, which a "scheme followed by non-space" rule gets
//!   wrong on almost every line of compiler output. Schemes that are legitimately opaque
//!   ([`OPAQUE_SCHEMES`]) are exempt.
//! * Trailing punctuation is trimmed the way autolinkers have converged on (Gruber, GitHub,
//!   `linkify`): sentence punctuation comes off, and a closing bracket comes off **only when it
//!   is unbalanced**. That is the difference between
//!   `…/wiki/Rust_(programming_language)` surviving intact and `(see https://x/a)` not keeping
//!   the paren that closed the prose.
//!
//! Everything here is pure and unit-tested; nothing in this module touches egui or a terminal.

use regex::Regex;

/// Schemes that are meaningful without an authority, so `scheme:body` is a link for them even
/// though there is no `//`. Everything else must be hierarchical — see the module docs.
const OPAQUE_SCHEMES: &[&str] = &[
    "mailto", "tel", "sms", "magnet", "news", "urn", "xmpp", "sip", "sips", "bitcoin", "geo",
    "matrix", "im", "callto", "facetime",
];

/// The schemes verterm recognises out of the box. Deliberately excludes `data:` and
/// `javascript:` (payload carriers with no business being clicked out of scrollback) and
/// anything else a handler might execute directly.
pub const DEFAULT_SCHEMES: &[&str] = &[
    "http", "https", "ftp", "ftps", "sftp", "file", "ssh", "git", "mailto", "tel", "sms", "magnet",
    "news", "irc", "ircs", "gemini", "gopher", "s3", "gs", "smb", "nfs", "dav", "davs", "webcal",
    "vnc", "rdp", "xmpp", "matrix", "ws", "wss", "vscode", "obsidian", "zoommtg",
];

/// RFC 3986's unencoded-safe characters, widened to non-ASCII for IRIs. See the module docs.
const BODY: &str =
    r"(?:[A-Za-z0-9\-._~:/?\#\[\]@!$&'()*+,;=%]|[^\x00-\x7F\u{A0}\u{2000}-\u{206F}\u{3000}])";

/// A DNS label, and a hostname of two or more of them (`example.com`, `a.b.co.uk`).
const LABEL: &str = r"[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?";

/// Where a match came from, which decides how [`LinkMatch::uri`] was built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detected {
    /// `scheme:…` — the URI is the matched text verbatim.
    Scheme,
    /// `www.host/…` — prefixed with `https://`.
    Www,
    /// `user@host.tld` — prefixed with `mailto:`.
    Email,
}

/// One link found in a single row of text. `start`/`end` are **char** indices into that row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkMatch {
    pub start: usize,
    /// Exclusive.
    pub end: usize,
    /// What is on screen.
    pub text: String,
    /// What to hand the opener.
    pub uri: String,
    pub how: Detected,
}

/// A compiled detector. Built once from `[hyperlinks]` and rebuilt on config reload, because
/// the scheme list is part of the pattern.
#[derive(Debug)]
pub struct Matcher {
    re: Regex,
    schemes: Vec<String>,
}

impl Default for Matcher {
    fn default() -> Self {
        let schemes: Vec<String> = DEFAULT_SCHEMES.iter().map(|s| s.to_string()).collect();
        Self::new(&schemes, true, true, true)
    }
}

impl Matcher {
    /// `schemes` are matched case-insensitively; unusable entries (anything that is not a valid
    /// RFC 3986 scheme) are dropped with a warning rather than failing startup.
    ///
    /// `detect` off builds a matcher that finds nothing but still [`allows`](Self::allows) the
    /// configured schemes — that is the state `detect = false` wants, where OSC 8 links keep
    /// working and only the guessing stops.
    pub fn new(schemes: &[String], detect: bool, www: bool, emails: bool) -> Self {
        let mut list: Vec<String> = Vec::new();
        for s in schemes {
            let s = s.trim().trim_end_matches(':').to_ascii_lowercase();
            if !is_scheme(&s) {
                tracing::warn!("[hyperlinks].schemes: {s:?} is not a URI scheme; ignoring");
                continue;
            }
            if !list.contains(&s) {
                list.push(s);
            }
        }
        // Longest first so the alternation reads unambiguously (`https` before `http`).
        let mut sorted = list.clone();
        sorted.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));

        let mut alts: Vec<String> = Vec::new();
        if detect && !sorted.is_empty() {
            let names = sorted
                .iter()
                .map(|s| regex::escape(s))
                .collect::<Vec<_>>()
                .join("|");
            alts.push(format!("(?:{names}):{BODY}+"));
        }
        if detect && www {
            alts.push(format!(r"www\.{LABEL}(?:\.{LABEL})+(?:[:/?\#]{BODY}*)?"));
        }
        if detect && emails {
            alts.push(format!(r"[A-Za-z0-9._%+\-]+@{LABEL}(?:\.{LABEL})+"));
        }
        // An empty alternation would match everywhere; a matcher with nothing enabled must
        // instead match nothing, so use a pattern that cannot succeed.
        let pattern = if alts.is_empty() {
            r"\A\z\B".to_string()
        } else {
            format!("(?i)(?:{})", alts.join("|"))
        };
        let re = Regex::new(&pattern).unwrap_or_else(|e| {
            tracing::warn!("[hyperlinks]: could not build the link pattern ({e}); using defaults");
            Regex::new(&format!(
                "(?i)(?:(?:{}):{BODY}+)",
                DEFAULT_SCHEMES.join("|")
            ))
            .expect("the built-in link pattern compiles")
        });
        Self { re, schemes: list }
    }

    /// Every link in one row, left to right, non-overlapping.
    pub fn find(&self, text: &str) -> Vec<LinkMatch> {
        let mut out = Vec::new();
        let mut char_of: Option<Vec<usize>> = None;
        for m in self.re.find_iter(text) {
            let before = text[..m.start()].chars().next_back();
            let Some((body, how)) = self.accept(m.as_str(), before) else {
                continue;
            };
            // Byte offsets are what the regex gives; grid work needs char indices. The map is
            // built lazily so rows without links never pay for it.
            let map = char_of.get_or_insert_with(|| {
                let mut v: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
                v.push(text.len());
                v
            });
            let to_char = |b: usize| map.partition_point(|&s| s < b);
            let start = to_char(m.start());
            let end = to_char(m.start() + body.len());
            let uri = match how {
                Detected::Scheme => body.to_string(),
                Detected::Www => format!("https://{body}"),
                Detected::Email => format!("mailto:{body}"),
            };
            out.push(LinkMatch {
                start,
                end,
                text: body.to_string(),
                uri,
                how,
            });
        }
        out
    }

    /// Validate one raw regex hit and trim it. `before` is the char preceding the match, which
    /// is how a match is rejected for starting in the middle of a word (there is no lookbehind
    /// in the `regex` crate, and the alternative — baking `(?:^|[^…])` into the pattern — would
    /// swallow that character and corrupt every offset).
    fn accept<'t>(&self, raw: &'t str, before: Option<char>) -> Option<(&'t str, Detected)> {
        let body = trim_trailing(raw);
        if body.is_empty() {
            return None;
        }
        if let Some((scheme, rest)) = split_scheme(body)
            && self.schemes.iter().any(|s| s == &scheme)
        {
            // `xhttps://…` is not a link, and neither is the `http` inside `a.http://b`.
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
                return None;
            }
            if let Some(after) = rest.strip_prefix("//") {
                let split = after.find(['/', '?', '#']).unwrap_or(after.len());
                let (authority, path) = after.split_at(split);
                // `https://` is a prefix, not a link, and neither is `https:///x`. The one
                // scheme whose authority may legitimately be empty is `file` (RFC 8089's
                // "local file" form, `file:///etc/hosts`) — and only if a path follows.
                if authority.is_empty() && !(scheme == "file" && path.len() > 1) {
                    return None;
                }
            } else if !OPAQUE_SCHEMES.contains(&scheme.as_str()) || rest.is_empty() {
                return None;
            }
            return Some((body, Detected::Scheme));
        }
        let lower = body.to_ascii_lowercase();
        if lower.starts_with("www.") {
            if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '@' | '_')) {
                return None;
            }
            return has_tld(host_of(body)).then_some((body, Detected::Www));
        }
        if let Some((local, host)) = body.split_once('@') {
            if before.is_some_and(|c| {
                c.is_alphanumeric() || matches!(c, '.' | '%' | '+' | '-' | '_' | '@')
            }) {
                return None;
            }
            // A dot-atom local part may not start or end with a dot (RFC 5322 §3.2.3).
            if local.starts_with('.') || local.ends_with('.') {
                return None;
            }
            return has_tld(host).then_some((body, Detected::Email));
        }
        None
    }

    /// Whether `uri`'s scheme is one this matcher was told to trust. Applied to **OSC 8** URIs
    /// too, which arrive straight from the program and never went through detection.
    pub fn allows(&self, uri: &str) -> bool {
        if uri.chars().any(|c| c.is_control()) {
            return false;
        }
        match split_scheme(uri) {
            Some((scheme, rest)) => !rest.is_empty() && self.schemes.iter().any(|s| s == &scheme),
            None => false,
        }
    }
}

/// Split `uri` at its first `:` when what precedes it is a valid RFC 3986 scheme. Returns the
/// lowercased scheme and the remainder.
fn split_scheme(uri: &str) -> Option<(String, &str)> {
    let colon = uri.find(':')?;
    let scheme = &uri[..colon];
    is_scheme(scheme).then(|| (scheme.to_ascii_lowercase(), &uri[colon + 1..]))
}

/// RFC 3986 §3.1: `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn is_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// The host part of a `www.`-style match: everything before the first `/`, `?`, `#` or `:`.
fn host_of(s: &str) -> &str {
    s.split(['/', '?', '#', ':']).next().unwrap_or(s)
}

/// A hostname is only worth linking when its last label reads as a TLD: two or more characters,
/// starting with a letter. This is what keeps `1.2.3.4`, `v1.2.3` and `Cargo.toml` out.
fn has_tld(host: &str) -> bool {
    let Some(tld) = host.rsplit('.').next() else {
        return false;
    };
    tld.chars().count() >= 2 && tld.starts_with(|c: char| c.is_alphabetic())
}

/// Punctuation that ends a sentence rather than a URI.
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', '\'', '"'];

/// Trim trailing prose punctuation, dropping a closing bracket only when it has no opener
/// inside the candidate. Iterated to a fixed point so `(https://x/a).` loses both.
fn trim_trailing(s: &str) -> &str {
    let mut end = s.len();
    while let Some(last) = s[..end].chars().next_back() {
        let unbalanced = |open: char, close: char| {
            let cur = &s[..end];
            cur.matches(close).count() > cur.matches(open).count()
        };
        let drop = match last {
            ')' => unbalanced('(', ')'),
            ']' => unbalanced('[', ']'),
            '}' => unbalanced('{', '}'),
            c => TRAILING.contains(&c),
        };
        if !drop {
            break;
        }
        end -= last.len_utf8();
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Matcher {
        Matcher::default()
    }

    fn texts(matcher: &Matcher, s: &str) -> Vec<String> {
        matcher.find(s).into_iter().map(|l| l.text).collect()
    }

    fn uris(matcher: &Matcher, s: &str) -> Vec<String> {
        matcher.find(s).into_iter().map(|l| l.uri).collect()
    }

    #[test]
    fn finds_plain_urls() {
        let m = m();
        assert_eq!(
            texts(&m, "see https://example.com/a?b=1#frag now"),
            ["https://example.com/a?b=1#frag"]
        );
        assert_eq!(
            texts(&m, "HTTPS://EXAMPLE.COM/A"),
            ["HTTPS://EXAMPLE.COM/A"]
        );
    }

    /// The bug a naive `trim_end_matches(')')` has: a closing paren that belongs to the URL.
    #[test]
    fn closing_bracket_is_kept_only_when_balanced() {
        let m = m();
        assert_eq!(
            texts(
                &m,
                "https://en.wikipedia.org/wiki/Rust_(programming_language)"
            ),
            ["https://en.wikipedia.org/wiki/Rust_(programming_language)"]
        );
        assert_eq!(
            texts(&m, "(see https://example.com/a)"),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "(see https://example.com/a)."),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "[https://example.com/a]"),
            ["https://example.com/a"]
        );
    }

    #[test]
    fn trims_sentence_punctuation_but_not_path_characters() {
        let m = m();
        assert_eq!(
            texts(&m, "go to https://example.com/a."),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "https://example.com/a, then"),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "https://example.com/a-"),
            ["https://example.com/a-"]
        );
        assert_eq!(
            texts(&m, "https://example.com/a_b~c"),
            ["https://example.com/a_b~c"]
        );
    }

    /// RFC 3986 excludes these outright, so the delimiters used to wrap URLs in prose and in
    /// shell commands terminate the match without any special case.
    #[test]
    fn rfc_excluded_delimiters_end_the_match() {
        let m = m();
        assert_eq!(
            texts(&m, "<https://example.com/a>"),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "curl \"https://example.com/a\" -o x"),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "curl 'https://example.com/a' | sh"),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "https://example.com/a`x"),
            ["https://example.com/a"]
        );
    }

    /// The rule that keeps compiler and log output from turning into a field of links.
    #[test]
    fn hierarchical_schemes_need_an_authority() {
        let m = m();
        assert!(texts(&m, "file: No such file or directory").is_empty());
        assert!(texts(&m, "git:reflog").is_empty());
        assert!(texts(&m, "https://").is_empty());
        assert_eq!(texts(&m, "file:///etc/hosts"), ["file:///etc/hosts"]);
    }

    #[test]
    fn opaque_schemes_do_not_need_slashes() {
        let m = m();
        assert_eq!(
            texts(&m, "write to mailto:ops@example.com now"),
            ["mailto:ops@example.com"]
        );
        assert_eq!(
            texts(&m, "magnet:?xt=urn:btih:abc"),
            ["magnet:?xt=urn:btih:abc"]
        );
    }

    #[test]
    fn unknown_schemes_are_not_links() {
        let m = m();
        assert!(texts(&m, "javascript://alert(1)").is_empty());
        assert!(texts(&m, "data://text/html,<b>x").is_empty());
        assert!(texts(&m, "custom://thing").is_empty());
    }

    #[test]
    fn a_scheme_inside_a_word_is_not_a_link() {
        let m = m();
        assert!(texts(&m, "xhttps://example.com").is_empty());
        assert!(texts(&m, "9http://example.com").is_empty());
        assert_eq!(texts(&m, "(http://example.com"), ["http://example.com"]);
    }

    #[test]
    fn www_and_email_are_normalised() {
        let m = m();
        assert_eq!(
            uris(&m, "visit www.example.com/x"),
            ["https://www.example.com/x"]
        );
        assert_eq!(
            uris(&m, "mail ops@example.com please"),
            ["mailto:ops@example.com"]
        );
    }

    #[test]
    fn version_strings_and_paths_are_not_hosts() {
        let m = m();
        assert!(texts(&m, "verterm v0.3.1 built").is_empty());
        assert!(texts(&m, "edit Cargo.toml and src/ui/mod.rs").is_empty());
        assert!(texts(&m, "host 10.0.0.7:22 up").is_empty());
        assert!(texts(&m, "id 123e4567-e89b-12d3-a456-426614174000").is_empty());
        // A numeric last label is not a TLD.
        assert!(texts(&m, "build user@1.2.3.4 ok").is_empty());
    }

    #[test]
    fn a_url_swallows_the_email_inside_it() {
        let m = m();
        assert_eq!(
            texts(&m, "https://example.com/?to=ops@example.com"),
            ["https://example.com/?to=ops@example.com"]
        );
    }

    #[test]
    fn several_links_on_one_row_keep_their_order_and_offsets() {
        let m = m();
        let row = "a https://x.example/1 b www.y.example c ops@z.example d";
        let found = m.find(row);
        assert_eq!(found.len(), 3);
        for l in &found {
            let got: String = row.chars().skip(l.start).take(l.end - l.start).collect();
            assert_eq!(got, l.text, "char offsets must address the text they name");
        }
        assert_eq!(found[0].how, Detected::Scheme);
        assert_eq!(found[1].how, Detected::Www);
        assert_eq!(found[2].how, Detected::Email);
    }

    #[test]
    fn char_offsets_survive_multibyte_text() {
        let m = m();
        let row = "日本語 https://example.com/ち end";
        let l = &m.find(row)[0];
        let got: String = row.chars().skip(l.start).take(l.end - l.start).collect();
        assert_eq!(got, l.text);
        assert_eq!(l.text, "https://example.com/ち");
    }

    /// Non-ASCII is allowed (IRIs), but the punctuation prose wraps URLs in is not.
    #[test]
    fn typographic_punctuation_is_not_part_of_a_url() {
        let m = m();
        assert_eq!(
            texts(&m, "“https://example.com/a”"),
            ["https://example.com/a"]
        );
        assert_eq!(
            texts(&m, "https://example.com/a—b"),
            ["https://example.com/a"]
        );
    }

    #[test]
    fn detection_toggles_are_honoured() {
        let all: Vec<String> = DEFAULT_SCHEMES.iter().map(|s| s.to_string()).collect();
        let no_extras = Matcher::new(&all, true, false, false);
        assert!(texts(&no_extras, "www.example.com ops@example.com").is_empty());
        assert_eq!(
            texts(&no_extras, "https://example.com"),
            ["https://example.com"]
        );

        let nothing = Matcher::new(&[], true, false, false);
        assert!(texts(&nothing, "https://example.com www.x.com a@b.com").is_empty());
    }

    #[test]
    fn a_custom_scheme_list_is_the_whole_list() {
        let only = Matcher::new(&["ssh".into(), "not a scheme".into()], true, false, false);
        assert_eq!(
            texts(&only, "ssh://box/ and https://x.example/"),
            ["ssh://box/"]
        );
    }

    /// `allows` is the gate for OSC 8, which never goes through `find`.
    #[test]
    fn allows_matches_the_configured_schemes() {
        let m = m();
        assert!(m.allows("https://example.com"));
        assert!(m.allows("mailto:a@b.com"));
        assert!(!m.allows("javascript:alert(1)"));
        assert!(!m.allows("data:text/html,x"));
        assert!(!m.allows("not-a-uri"));
        assert!(!m.allows("https:"));
        assert!(!m.allows("https://x\nmailto:y"));
    }

    #[test]
    fn scheme_grammar_follows_rfc_3986() {
        assert!(is_scheme("http"));
        assert!(is_scheme("view-source"));
        assert!(is_scheme("a+b.c-d"));
        assert!(!is_scheme(""));
        assert!(!is_scheme("1http"));
        assert!(!is_scheme("ht tp"));
        assert!(!is_scheme("ht_tp"));
    }
}

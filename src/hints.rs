//! Fast-jump hints: scan the visible grid for URLs, file paths, IPs, UUIDs and git hashes,
//! then label each with a home-row tag. Pure logic — no egui here.
//!
//! URLs are **not** matched here. They come from [`crate::links::Matcher`], so that the thing
//! hint mode will open and the thing a click will open are decided by one piece of code
//! obeying one `[hyperlinks]` configuration — two URL patterns drifting apart is exactly how a
//! terminal ends up highlighting a link it then refuses to open.

use std::sync::LazyLock;

use regex::Regex;

use crate::links::Matcher;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HintKind {
    Url,
    Path,
    Ip,
    Uuid,
    GitHash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HintMatch {
    pub row: usize,
    pub col_start: usize,
    /// Exclusive.
    pub col_end: usize,
    pub text: String,
    pub kind: HintKind,
    /// For [`HintKind::Url`], the URI to open — `www.x` normalised to `https://www.x`, a bare
    /// address to `mailto:`. `None` for every other kind, whose text *is* the payload.
    pub uri: Option<String>,
}

/// One rendered terminal row: the text plus, for every char, the grid column it occupies
/// (wide characters skip a column; zero-width marks share one).
#[derive(Clone, Debug, Default)]
pub struct RowText {
    pub text: String,
    pub cols: Vec<usize>,
}

impl RowText {
    pub fn push(&mut self, c: char, col: usize) {
        self.text.push(c);
        self.cols.push(col);
    }

    pub fn trim_end(&mut self) {
        while self.text.ends_with(' ') {
            self.text.pop();
            self.cols.pop();
        }
    }
}

struct Pattern {
    kind: HintKind,
    re: Regex,
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let p = |kind, re: &str| Pattern {
        kind,
        re: Regex::new(re).expect("hint regex"),
    };
    vec![
        p(
            HintKind::Uuid,
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        ),
        p(
            HintKind::Ip,
            r"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b|\b(?:[0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}\b",
        ),
        p(
            HintKind::Path,
            r#"(?:~|\.{1,2})?/(?:[\w.@%+=,-]+/)*[\w.@%+=,-]+(?::\d+(?::\d+)?)?|\b[\w-]+(?:/[\w.@%+=,-]+)+(?::\d+(?::\d+)?)?|\b[\w-]+\.(?:rs|toml|md|py|go|c|h|cc|cpp|hpp|js|ts|tsx|jsx|json|ya?ml|txt|log|sh|fish|zsh|conf|cfg|ini|service|lock|sql|html|css|xml|csv)(?::\d+(?::\d+)?)?\b"#,
        ),
        p(HintKind::GitHash, r"\b[0-9a-f]{7,40}\b"),
    ]
});

/// Find all hints in the visible rows. Overlapping matches are resolved by priority — links
/// first, then UUIDs before the git hashes they would otherwise be mistaken for.
pub fn find_hints(rows: &[RowText], links: &Matcher) -> Vec<HintMatch> {
    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        if row.text.trim().is_empty() {
            continue;
        }
        // Byte offset → char index → grid column.
        let char_starts: Vec<usize> = row.text.char_indices().map(|(b, _)| b).collect();
        let byte_to_char = |b: usize| char_starts.partition_point(|&s| s < b);
        let col = |cs: usize, ce: usize| {
            (
                row.cols.get(cs).copied().unwrap_or(cs),
                row.cols.get(ce - 1).map(|c| c + 1).unwrap_or(ce),
            )
        };
        let mut taken: Vec<(usize, usize)> = Vec::new(); // char ranges already claimed
        // Links claim their span before anything else runs, so a path or hash pattern can
        // never carve a piece out of the middle of a URL.
        for lm in links.find(&row.text) {
            let (col_start, col_end) = col(lm.start, lm.end);
            taken.push((lm.start, lm.end));
            out.push(HintMatch {
                row: row_idx,
                col_start,
                col_end,
                text: lm.text,
                kind: HintKind::Url,
                uri: Some(lm.uri),
            });
        }
        for pat in PATTERNS.iter() {
            for m in pat.re.find_iter(&row.text) {
                let (cs, ce) = (byte_to_char(m.start()), byte_to_char(m.end()));
                if ce <= cs {
                    continue;
                }
                if taken.iter().any(|&(s, e)| cs < e && ce > s) {
                    continue;
                }
                let text = m
                    .as_str()
                    .trim_end_matches(['.', ',', ';', ':', ')'])
                    .to_string();
                if text.is_empty()
                    || (pat.kind == HintKind::GitHash && text.chars().all(|c| c.is_ascii_digit()))
                {
                    continue;
                }
                let ce = cs + text.chars().count();
                taken.push((cs, ce));
                let (col_start, col_end) = col(cs, ce);
                out.push(HintMatch {
                    row: row_idx,
                    col_start,
                    col_end,
                    text,
                    kind: pat.kind,
                    uri: None,
                });
            }
        }
    }
    out.sort_by_key(|h| (h.row, h.col_start));
    out
}

const TAG_ALPHABET: &[u8] = b"asdfghjklqwertyuiopzxcvbnm";

/// Tags for `n` hints: single letters while they last, then two-letter combos. Generated so
/// that no single-letter tag is a prefix of a two-letter one when both are in use.
pub fn assign_tags(n: usize) -> Vec<String> {
    let a = TAG_ALPHABET.len();
    if n <= a {
        return TAG_ALPHABET[..n]
            .iter()
            .map(|&c| (c as char).to_string())
            .collect();
    }
    // Reserve `k` leading letters as two-letter prefixes so that singles + doubles cover n.
    let mut k = 1;
    while (a - k) + k * a < n && k < a {
        k += 1;
    }
    let mut tags: Vec<String> = TAG_ALPHABET[k..]
        .iter()
        .map(|&c| (c as char).to_string())
        .collect();
    for &p in &TAG_ALPHABET[..k] {
        for &s in TAG_ALPHABET {
            tags.push(format!("{}{}", p as char, s as char));
        }
    }
    tags.truncate(n);
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> Matcher {
        Matcher::default()
    }

    fn row(s: &str) -> RowText {
        let mut r = RowText::default();
        for (i, c) in s.chars().enumerate() {
            r.push(c, i);
        }
        r
    }

    #[test]
    fn finds_mixed_hints() {
        let rows = vec![row(
            "see https://example.com/a?b=1, /etc/nginx/nginx.conf:42 host 10.0.0.7:22 id 123e4567-e89b-12d3-a456-426614174000 at deadbeefcafe",
        )];
        let hints = find_hints(&rows, &links());
        let kinds: Vec<_> = hints.iter().map(|h| h.kind).collect();
        assert_eq!(
            kinds,
            vec![
                HintKind::Url,
                HintKind::Path,
                HintKind::Ip,
                HintKind::Uuid,
                HintKind::GitHash
            ]
        );
        assert_eq!(hints[0].text, "https://example.com/a?b=1");
        assert_eq!(hints[1].text, "/etc/nginx/nginx.conf:42");
        assert_eq!(hints[2].text, "10.0.0.7:22");
    }

    #[test]
    fn relative_paths_and_files() {
        let hints = find_hints(
            &[row("error in src/ui/mod.rs:120:5 and Cargo.toml")],
            &links(),
        );
        let texts: Vec<_> = hints.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, vec!["src/ui/mod.rs:120:5", "Cargo.toml"]);
    }

    #[test]
    fn columns_follow_wide_chars() {
        // "日本 /tmp" — the two CJK chars occupy columns 0-1 and 2-3.
        let mut r = RowText::default();
        r.push('日', 0);
        r.push('本', 2);
        r.push(' ', 4);
        for (i, c) in "/tmp".chars().enumerate() {
            r.push(c, 5 + i);
        }
        let hints = find_hints(&[r], &links());
        assert_eq!(hints.len(), 1);
        assert_eq!((hints[0].col_start, hints[0].col_end), (5, 9));
    }

    /// Hint mode must reach a URL by the same rules a click does, `uri` included — that is the
    /// whole point of delegating to `links::Matcher` instead of keeping a second pattern here.
    #[test]
    fn url_hints_carry_the_normalised_uri() {
        let hints = find_hints(&[row("ping www.example.com or ops@example.com")], &links());
        let pairs: Vec<_> = hints
            .iter()
            .map(|h| (h.kind, h.text.as_str(), h.uri.as_deref()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (
                    HintKind::Url,
                    "www.example.com",
                    Some("https://www.example.com")
                ),
                (
                    HintKind::Url,
                    "ops@example.com",
                    Some("mailto:ops@example.com")
                ),
            ]
        );
    }

    /// The path and git-hash patterns are hungry; a URL claims its span first so neither can
    /// take a bite out of the middle of one.
    #[test]
    fn a_url_is_never_carved_up_by_the_other_patterns() {
        let hints = find_hints(
            &[row(
                "fetch https://example.com/deadbeefcafe/src/ui/mod.rs now",
            )],
            &links(),
        );
        let texts: Vec<_> = hints.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["https://example.com/deadbeefcafe/src/ui/mod.rs"]
        );
    }

    #[test]
    fn tags_are_prefix_free() {
        for n in [1, 5, 26, 27, 60, 200] {
            let tags = assign_tags(n);
            assert_eq!(tags.len(), n);
            for (i, a) in tags.iter().enumerate() {
                for (j, b) in tags.iter().enumerate() {
                    if i != j {
                        assert!(!b.starts_with(a.as_str()), "{a} is a prefix of {b} (n={n})");
                    }
                }
            }
        }
    }
}

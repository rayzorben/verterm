//! Finding a session in the rail: matching on what identifies it, and — on a worker thread —
//! on what its scrollback contains.
//!
//! The identity half is pure and lives here so it can be tested without a terminal. The
//! content half is a bounded search over each tab's grid, run off the GUI thread because it
//! takes the `Term` lock of every session (hard rule 1).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::RegexSearch;
use parking_lot::FairMutex;

use crate::session::{EventProxy, TabId};

/// Below this the query matches nearly everything, so the scrollback scan is pure waste; the
/// identity list alone is a better answer while the user is still typing the first letters.
pub const MIN_CONTENT_QUERY: usize = 3;
/// Lines of scrollback searched per tab, newest first. A full 10 000-line history per tab per
/// keystroke is the cost this cap exists to bound; a match older than this is not what someone
/// typing into a filter box is looking for.
pub const CONTENT_SCAN_LINES: usize = 4000;

/// Everything about a session a user might type to find it again. Assembled on the GUI thread
/// from live state, then matched purely.
#[derive(Clone, Debug, Default)]
pub struct SessionFields {
    /// 1-based rail position, i.e. what `Alt+N` would select.
    pub index: usize,
    pub title: String,
    /// Working directory, already shortened to `~/…`.
    pub cwd: String,
    /// Shell or program the tab was started with.
    pub program: String,
    /// Foreground command, when one is running.
    pub foreground: String,
    /// Group the tab is filed under.
    pub group: String,
    /// SSH host or container name.
    pub host: String,
    pub branch: String,
    /// Words describing the kind: `ssh remote`, `container`, `root elevated`, `scratch`.
    pub kind: &'static str,
}

/// How well a session matched, lower being better. The order is deliberate: what the user sees
/// in the row (title, cwd) ranks above what they do not (group, kind words), so typing what is
/// on screen finds that row first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Score(pub u32);

/// Score `q` (already lowercased) against one session, or `None` if nothing matched. An empty
/// query matches nothing rather than everything — `starts_with("")` is true for every field,
/// so without this guard a cleared search box would rank every session as a near-exact hit.
pub fn identity_score(q: &str, f: &SessionFields) -> Option<Score> {
    if q.is_empty() {
        return None;
    }
    // `Alt+N` is how tabs are selected, so a bare digit means that tab, exactly.
    if let Ok(n) = q.parse::<usize>()
        && n == f.index
    {
        return Some(Score(0));
    }
    let rank = |hay: &str, base: u32| -> Option<u32> {
        let hay = hay.to_lowercase();
        if hay.is_empty() {
            return None;
        }
        if hay == q {
            Some(base)
        } else if hay.starts_with(q) {
            Some(base + 1)
        } else if hay.contains(q) {
            Some(base + 2)
        } else {
            None
        }
    };
    // Fields the user can actually see on the row come first.
    [
        rank(&f.title, 10),
        rank(&f.host, 20),
        rank(&f.cwd, 30),
        rank(&f.foreground, 40),
        rank(&f.branch, 50),
        rank(&f.group, 60),
        rank(&f.program, 70),
        rank(f.kind, 80),
    ]
    .into_iter()
    .flatten()
    .min()
    .map(Score)
}

/// A session whose scrollback contains the query, with the line it was found on.
#[derive(Clone, Debug)]
pub struct ContentHit {
    pub id: TabId,
    /// The matching line, trimmed, for the result row's second line.
    pub preview: String,
}

/// One content search: the tabs to scan and where to send the answer.
pub struct ContentSearch {
    /// Bumped per query; a reply carrying an older generation is stale and dropped.
    pub generation: u64,
    pub query: String,
    pub tabs: Vec<(TabId, Arc<FairMutex<Term<EventProxy>>>)>,
}

/// Run a content search on a worker thread and stream the answer back.
///
/// One thread per query rather than a pool: a search is short and bounded, queries are typed by
/// a human, and `cancel` lets an in-flight one give up between tabs the moment the query moves
/// on. Every reply carries its generation so a slow one can never overwrite a newer answer.
pub fn spawn(
    search: ContentSearch,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
) -> Receiver<(u64, Vec<ContentHit>)> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("tab-search".into())
        .spawn(move || {
            let hits = run(&search, &cancel);
            if !cancel.load(Ordering::Relaxed) {
                let _ = tx.send((search.generation, hits));
                ctx.request_repaint();
            }
        })
        .expect("spawn tab-search thread");
    rx
}

fn run(search: &ContentSearch, cancel: &AtomicBool) -> Vec<ContentHit> {
    // The query is literal text, not a regex — escape it, exactly as the vi search bar does.
    let Ok(mut regex) = RegexSearch::new(&regex::escape(&search.query)) else {
        return Vec::new();
    };
    let mut hits = Vec::new();
    for (id, term) in &search.tabs {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // The lock is shared with the GUI thread and the reader thread, so it is taken once
        // per tab and released before moving on — never held across the whole search.
        let t = term.lock();
        if let Some(preview) = first_match(&t, &mut regex) {
            hits.push(ContentHit { id: *id, preview });
        }
    }
    hits
}

/// Search one grid backwards from the bottom and return the text of the first match's line.
fn first_match(term: &Term<EventProxy>, regex: &mut RegexSearch) -> Option<String> {
    let grid = term.grid();
    let origin = Point::new(Line(grid.screen_lines() as i32 - 1), grid.last_column());
    let m = term.search_next(
        regex,
        origin,
        Direction::Left,
        Side::Right,
        Some(CONTENT_SCAN_LINES),
    )?;
    let line = m.start().line;
    let text = term.bounds_to_string(
        Point::new(line, Column(0)),
        Point::new(line, grid.last_column()),
    );
    Some(text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> SessionFields {
        SessionFields {
            index: 3,
            title: "verterm".into(),
            cwd: "~/Dropbox/PC/development/verterm".into(),
            program: "bash".into(),
            foreground: "cargo".into(),
            group: "verterm".into(),
            host: String::new(),
            branch: "main".into(),
            kind: "local",
        }
    }

    #[test]
    fn a_bare_digit_selects_that_tab_the_way_alt_n_would() {
        assert_eq!(identity_score("3", &fields()), Some(Score(0)));
        // …and only that tab.
        assert_eq!(identity_score("4", &fields()), None);
    }

    #[test]
    fn what_is_visible_on_the_row_outranks_what_is_not() {
        let f = SessionFields {
            title: "notes".into(),
            group: "notes".into(),
            ..fields()
        };
        // Both fields hold the query; the title wins, so typing what you can see finds it.
        assert_eq!(identity_score("notes", &f), Some(Score(10)));
        // An exact match beats a prefix beats a substring, within the same field.
        assert!(identity_score("note", &f).unwrap() > identity_score("notes", &f).unwrap());
        assert!(identity_score("ote", &f).unwrap() > identity_score("note", &f).unwrap());
    }

    #[test]
    fn a_session_is_findable_by_every_field_a_user_might_remember() {
        let f = SessionFields {
            host: "orohost".into(),
            kind: "ssh remote",
            ..fields()
        };
        for q in [
            "verterm",     // title
            "orohost",     // host
            "development", // cwd
            "cargo",       // foreground
            "main",        // branch
            "bash",        // program
            "ssh",         // kind word
            "remote",      // the other kind word
        ] {
            assert!(identity_score(q, &f).is_some(), "{q:?} found nothing");
        }
        assert_eq!(identity_score("nothing-like-this", &f), None);
    }

    #[test]
    fn empty_query_and_empty_fields_both_match_nothing() {
        // `starts_with("")` is true for every field, so an empty query would otherwise rank
        // every session as a near-exact hit.
        assert_eq!(identity_score("", &fields()), None);
        // And an empty haystack must not be treated as containing a query.
        let f = SessionFields {
            title: String::new(),
            cwd: String::new(),
            program: String::new(),
            foreground: String::new(),
            group: String::new(),
            host: String::new(),
            branch: String::new(),
            kind: "",
            ..fields()
        };
        assert_eq!(identity_score("x", &f), None);
    }
}

//! The eframe application: owns sessions and the keymap, runs the per-frame pipeline
//! (poll sessions → global chords → rail → terminal → overlays) and applies every action.
//! Submodules are presentation-only; all state changes happen here.

pub mod chrome;
pub mod fonts;
pub mod overlays;
pub mod rail;
pub mod terminal_view;
pub mod theme;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Boundary, Direction, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::vi_mode::ViMotion;
use egui::{Align2, CornerRadius, Event, Key, Modifiers, Rect, Sense, Stroke, pos2, vec2};

use crate::Cli;
use crate::ai::AiClient;
use crate::config::{Config, LinkActivate, RailSide, UiState};
use crate::hints::{self, HintKind, HintMatch};
use crate::input;
use crate::keymap::{Action, Chord, Keymap};
use crate::links::Matcher;
use crate::notify;
use crate::procscan::{self, ScanRegistry};
use crate::session::{
    ProcessCategory, Session, SessionState, SpawnRequest, TabId, TabKind, default_shell,
};
use crate::shell_integration::ShellIntegration;
use crate::tabsearch;
use crate::workspace::{self, Bucket, TabInput, TabTree};
use chrome::{Chip, Typography};
use fonts::{CellMetrics, TermFonts};
use overlays::{
    AiChrome, AiOutcome, AiOverlayState, PaletteOutcome, PaletteState, PromptOutcome, PromptState,
    SearchOutcome,
};
use rail::{Badge, Indicator, Row, TabRow};
use terminal_view::{GridLink, HintOverlay, LinkOverlay, RenderCtx};
use theme::{Palette, UiColors};

// ---------------------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------------------

pub struct SearchState {
    pub query: String,
    pub backwards: bool,
    pub origin: Point,
    pub regex: Option<RegexSearch>,
    pub current: Option<Match>,
    /// True while the search bar owns the keyboard.
    pub input_active: bool,
}

#[derive(Default)]
pub struct ViState {
    pub search: Option<SearchState>,
}

pub struct HintsState {
    pub matches: Vec<HintMatch>,
    pub tags: Vec<String>,
    pub typed: String,
}

pub enum Mode {
    Normal,
    Vi(Box<ViState>),
    Hints(HintsState),
    Ai(Box<AiOverlayState>),
    Palette(PaletteState),
    Prompt(PromptState),
    /// The rail's find box owns the keyboard.
    FindSession,
}

impl Mode {
    fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Vi(v) if v.search.is_some() => "SEARCH",
            Mode::Vi(_) => "VI",
            Mode::Hints(_) => "HINTS",
            Mode::Ai(_) => "AI",
            Mode::Palette(_) => "PALETTE",
            Mode::Prompt(_) => "SCRATCH",
            Mode::FindSession => "FIND",
        }
    }

    /// True when an egui text field owns keyboard input (so the terminal must not drain it).
    fn text_input_active(&self) -> bool {
        match self {
            Mode::Ai(_) | Mode::Palette(_) | Mode::Prompt(_) | Mode::FindSession => true,
            Mode::Vi(v) => v.search.as_ref().is_some_and(|s| s.input_active),
            _ => false,
        }
    }
}

/// Deferred work produced while a `&mut self.mode` borrow is alive.
enum Followup {
    None,
    AiSubmit(String),
    AiInsert(String),
    AiExecute(String),
    AiMoved((f32, f32)),
    CloseMode,
    Run(Action),
    SpawnScratch(String),
    SearchChanged,
    SearchConfirm,
    SearchCancel,
}

/// What the terminal was showing where the user right-clicked. Captured once, when the menu
/// opens, so the items stay meaningful while output keeps scrolling behind the popup.
#[derive(Clone, Debug, Default)]
struct TermMenuContext {
    /// Current selection, if it is non-empty.
    selection: Option<String>,
    /// URL / path / IP / hash under the click, for "Open" and "Copy" without hint mode.
    hint: Option<HintMatch>,
    /// Hyperlink under the click (OSC 8 or detected). Separate from `hint` because an OSC 8
    /// link's *label* need not look like a URL at all, so the pattern scan cannot find it.
    link: Option<GridLink>,
    /// Working directory of the tab, for "New tab here".
    cwd: Option<PathBuf>,
}

/// One chosen terminal-menu item, applied after the menu closure returns.
#[derive(Clone, Copy, Debug)]
enum TermMenuAction {
    Copy,
    Paste,
    OpenLink,
    CopyLink,
    OpenHint,
    CopyHint,
    SelectAll,
    NewTabHere,
    Run(Action),
}

/// The rail's find box and the answer to whatever is typed in it.
#[derive(Default)]
struct FindState {
    query: String,
    /// Which result row is selected, as an index into the flattened result list.
    selected: usize,
    /// Sessions whose scrollback contains the query. Identity matching is instant and done
    /// inline; this half arrives from a worker thread.
    content: Vec<tabsearch::ContentHit>,
    /// Bumped per query. A reply carrying an older generation lost the race and is dropped.
    generation: u64,
    /// Generation the `content` above answers, so a stale reply cannot overwrite a newer one.
    answered: u64,
    rx: Option<Receiver<(u64, Vec<tabsearch::ContentHit>)>>,
    cancel: Arc<AtomicBool>,
    /// When the query last changed. The scrollback scan waits for typing to settle rather than
    /// firing a pass per keystroke.
    changed_at: Option<Instant>,
}

/// How long the query must hold still before the scrollback scan runs.
///
/// Ordinary typing lands a character every 120–200 ms, so a shorter window does not debounce
/// anything — measured at 180 ms it fired six scans for an eight-letter word. 300 ms collapses
/// a typed word into one pass while still feeling immediate, and costs nothing in perceived
/// latency because the identity matches (the common case) are computed inline every frame.
const FIND_DEBOUNCE: Duration = Duration::from_millis(300);

impl FindState {
    fn clear(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        *self = Self::default();
    }
}

/// Which chrome colour a kind mark takes. Kept as a token rather than a `Color32` so the
/// mapping stays pure and unit-testable — the palette is only known to the running App.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IconTint {
    Muted,
    Gold,
    Blue,
    Vermilion,
    Amber,
}

/// The colour of a session's mark, from what it is. Only the kinds that earn attention get a
/// colour of their own; an ordinary local shell stays `muted` so the coloured ones stand out.
fn icon_tint(category: &ProcessCategory, kind: TabKind) -> IconTint {
    match category {
        ProcessCategory::ElevatedRoot => IconTint::Vermilion,
        ProcessCategory::RemoteSSH { .. } => IconTint::Gold,
        ProcessCategory::Container { .. } => IconTint::Blue,
        ProcessCategory::Local if kind == TabKind::Ephemeral => IconTint::Amber,
        ProcessCategory::Local => IconTint::Muted,
    }
}

/// The mark for a group key. Tabs use it too, via their *natural* key, so a row and the header
/// above it can never disagree about what kind of thing they are. `local:~` is the only home;
/// `local:/` is a directory outside `$HOME` with no project root, which is still a folder.
fn group_icon(key: &str) -> chrome::Icon {
    if key.starts_with("remote:") {
        chrome::Icon::Globe
    } else if key.starts_with("container:") {
        chrome::Icon::Box
    } else if key == "elevated" {
        chrome::Icon::Shield
    } else if key == "ephemeral" {
        chrome::Icon::Bolt
    } else if key == "local:~" {
        chrome::Icon::Home
    } else {
        chrome::Icon::Folder
    }
}

fn bucket_icon(bucket: Bucket) -> chrome::Icon {
    match bucket {
        Bucket::Local => chrome::Icon::Folder,
        Bucket::Remote => chrome::Icon::Globe,
        Bucket::Container => chrome::Icon::Box,
        Bucket::Elevated => chrome::Icon::Shield,
        Bucket::Ephemeral => chrome::Icon::Bolt,
    }
}

/// The hue a bucket's panel is washed with. It is the same colour the sessions inside it
/// already wear on their identity chips — gold for an ssh host, blue for a container,
/// vermilion for root — so the category box and its rows say the same thing.
fn bucket_tint(bucket: Bucket, c: &UiColors) -> egui::Color32 {
    match bucket {
        Bucket::Local => c.accent,
        Bucket::Remote => c.gold,
        Bucket::Container => c.blue,
        Bucket::Elevated => c.vermilion,
        Bucket::Ephemeral => c.muted,
    }
}

/// Menu labels quote the matched text, which can be a 200-char URL; keep the popup narrow.
fn elide_menu_text(s: &str) -> String {
    const MAX: usize = 32;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX - 1).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------------------

pub struct App {
    cfg: Config,
    keymap: Keymap,
    palette: Arc<Palette>,
    colors: UiColors,
    sessions: Vec<Session>,
    active: Option<TabId>,
    next_id: TabId,
    collapsed: HashSet<String>,
    rail_visible: bool,
    registry: Arc<ScanRegistry>,
    integration: Option<Arc<ShellIntegration>>,
    home: PathBuf,
    term_fonts: TermFonts,
    font_size: f32,
    metrics: CellMetrics,
    mode: Mode,
    ai: Option<AiClient>,
    super_down: bool,
    window_focused: bool,
    last_focus_state: Option<(bool, Option<TabId>)>,
    term_rect: Rect,
    selection_drag: bool,
    last_click: Option<(Instant, egui::Pos2, u8)>,
    /// Grid context captured when the terminal context menu was opened. Sampled at click
    /// time because the menu outlives the frame and the grid keeps scrolling underneath it.
    term_menu: TermMenuContext,
    scroll_accum: f32,
    last_title: String,
    toasts: Vec<(Instant, String)>,
    config_rx: Option<Receiver<Config>>,
    /// `--theme <name>`, if it was given. Re-applied on every config reload: the flag is a
    /// property of this run, so editing config.toml must not silently take the theme back.
    theme_override: Option<String>,
    /// Pending context-menu paste: (tab, clipboard text) from the reader thread.
    paste_rx: Option<Receiver<(TabId, String)>>,
    /// The question behind the last AI suggestion the user *ran*, so the overlay can offer to
    /// re-ask it once that command has failed.
    ai_last_question: Option<String>,
    /// Layout the user moved with the mouse, persisted across restarts.
    ui_state: UiState,
    /// "Show session" clicked on a desktop notification: the tab it came from.
    notify_tx: Sender<TabId>,
    notify_rx: Receiver<TabId>,
    /// The rail's find box. Lives on the App rather than in `Mode` because the box stays on
    /// screen (and keeps its query) whether or not it currently owns the keyboard.
    find: FindState,
    /// Compiled from `[hyperlinks]`; rebuilt on config reload because the scheme allowlist is
    /// baked into the pattern.
    link_matcher: Matcher,
    /// Links on the active tab's visible grid, recomputed only when the grid or the view
    /// actually moved — see `sync_links`.
    links: Vec<GridLink>,
    /// What `links` was computed from: tab, grid generation, scroll position, size.
    links_key: Option<(TabId, u64, usize, u16, u16)>,
    /// Index into `links` of the one under the pointer this frame.
    hovered_link: Option<usize>,
    /// The link the primary button went down on. Activation waits for the release on the same
    /// link with nothing selected, so pressing inside a URL to drag a selection still selects.
    link_press: Option<usize>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, cli: Cli) -> anyhow::Result<Self> {
        let ctx = cc.egui_ctx.clone();
        let (notify_tx, notify_rx) = mpsc::channel();
        let scheme = cfg.colors.resolve();
        let palette = Arc::new(Palette::from_scheme(&scheme));
        let colors = UiColors::from_scheme(&scheme);
        ctx.set_visuals(theme::visuals(&colors));
        let term_fonts = fonts::install(&ctx, &cfg.font);
        tracing::info!(font = %term_fonts.description, "terminal font family");
        let font_size = cfg.font.size.clamp(6.0, 48.0);
        // Fonts are not laid out until egui's first frame; start with an estimate and let
        // `App::ui` measure the real cell size every frame (it is cheap).
        let metrics = fonts::estimate(font_size, cfg.font.line_height);

        let mut keymap = Keymap::defaults();
        let warnings = keymap.apply_config(&cfg.keys);

        let config_rx = cli
            .config
            .clone()
            .or_else(Config::default_path)
            .map(|path| Config::spawn_watcher(path, ctx.clone()));

        let home = directories::BaseDirs::new()
            .map(|b| b.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        let registry = ScanRegistry::new();
        procscan::spawn_scanner(
            registry.clone(),
            ctx.clone(),
            Duration::from_millis(cfg.general.scan_interval_ms.max(100)),
            home.clone(),
        );

        let integration = if cfg.general.shell_integration {
            match ShellIntegration::install() {
                Ok(si) => {
                    tracing::info!(dir = %si.dir().display(), "shell integration installed");
                    Some(Arc::new(si))
                }
                Err(e) => {
                    tracing::warn!("shell integration unavailable: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        let link_matcher = cfg.hyperlinks.matcher();

        let ai = if cfg.ai.enabled {
            match AiClient::new(cfg.ai.clone()) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("AI client disabled: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        let rail_visible = cfg.general.rail_visible;
        let mut app = Self {
            cfg,
            keymap,
            palette,
            colors,
            sessions: Vec::new(),
            active: None,
            next_id: 1,
            collapsed: HashSet::new(),
            rail_visible,
            registry,
            integration,
            home,
            term_fonts,
            font_size,
            metrics,
            mode: Mode::Normal,
            ai,
            super_down: false,
            window_focused: true,
            last_focus_state: None,
            term_rect: Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 0.0)),
            selection_drag: false,
            last_click: None,
            term_menu: TermMenuContext::default(),
            ai_last_question: None,
            ui_state: UiState::load(),
            notify_tx,
            notify_rx,
            find: FindState::default(),
            scroll_accum: 0.0,
            last_title: String::new(),
            toasts: Vec::new(),
            config_rx,
            theme_override: cli.theme.clone(),
            paste_rx: None,
            link_matcher,
            links: Vec::new(),
            links_key: None,
            hovered_link: None,
            link_press: None,
        };
        for w in warnings {
            app.toast(w);
        }

        let (program, args) = if cli.command.is_empty() {
            (None, Vec::new())
        } else {
            (Some(cli.command[0].clone()), cli.command[1..].to_vec())
        };
        if app
            .spawn_tab(
                &ctx,
                program,
                args,
                cli.working_directory.clone(),
                TabKind::Normal,
            )
            .is_none()
        {
            anyhow::bail!("could not start the first shell");
        }
        Ok(app)
    }

    // -----------------------------------------------------------------------------------
    // Session helpers
    // -----------------------------------------------------------------------------------

    fn toast(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!("{msg}");
        self.toasts.push((Instant::now(), msg));
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    /// Drain the config-watch channel and apply the most recent reload, if any queued up.
    fn poll_config_reload(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.config_rx else {
            return;
        };
        if let Some(cfg) = rx.try_iter().last() {
            self.apply_config(ctx, cfg);
        }
    }

    /// Re-derive everything cached from config (theme, fonts, keymap, AI client) and swap in
    /// the new values. Runtime-only state the user may have adjusted since startup — rail
    /// visibility, zoomed font size, open tabs — is left alone. `general.scan_interval_ms` and
    /// `general.shell_integration` are read only at startup and need a restart to take effect.
    fn apply_config(&mut self, ctx: &egui::Context, mut cfg: Config) {
        if let Some(name) = &self.theme_override {
            cfg.colors.theme = Some(name.clone());
        }
        let scheme = cfg.colors.resolve();
        self.palette = Arc::new(Palette::from_scheme(&scheme));
        self.colors = UiColors::from_scheme(&scheme);
        ctx.set_visuals(theme::visuals(&self.colors));

        if cfg.font.family != self.cfg.font.family || cfg.font.ui_family != self.cfg.font.ui_family
        {
            self.term_fonts = fonts::install(ctx, &cfg.font);
            tracing::info!(font = %self.term_fonts.description, "terminal font family reloaded");
        }

        self.keymap = Keymap::defaults();
        let warnings = self.keymap.apply_config(&cfg.keys);

        let ai_changed = cfg.ai.enabled != self.cfg.ai.enabled
            || cfg.ai.endpoint != self.cfg.ai.endpoint
            || cfg.ai.insecure_tls != self.cfg.ai.insecure_tls
            || cfg.ai.timeout_secs != self.cfg.ai.timeout_secs;
        if ai_changed {
            self.ai = if cfg.ai.enabled {
                match AiClient::new(cfg.ai.clone()) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::warn!("AI client disabled: {e:#}");
                        None
                    }
                }
            } else {
                None
            };
        }

        if cfg.hyperlinks != self.cfg.hyperlinks {
            self.link_matcher = cfg.hyperlinks.matcher();
            self.links.clear();
            self.links_key = None;
            self.hovered_link = None;
        }

        self.cfg = cfg;
        self.toast("config reloaded");
        for w in warnings {
            self.toast(w);
        }
        ctx.request_repaint();
    }

    fn active_index(&self) -> Option<usize> {
        let id = self.active?;
        self.sessions.iter().position(|s| s.id == id)
    }

    fn session(&self, id: TabId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn grid_dims(&self) -> (u16, u16) {
        if self.term_rect.width() > self.metrics.w && self.term_rect.height() > self.metrics.h {
            (
                ((self.term_rect.width() / self.metrics.w).floor() as u16).max(2),
                ((self.term_rect.height() / self.metrics.h).floor() as u16).max(1),
            )
        } else {
            (80, 24)
        }
    }

    fn cell_px(&self, ctx: &egui::Context) -> (u16, u16) {
        let ppp = ctx.pixels_per_point();
        (
            ((self.metrics.w * ppp).round() as u16).max(1),
            ((self.metrics.h * ppp).round() as u16).max(1),
        )
    }

    fn spawn_tab(
        &mut self,
        ctx: &egui::Context,
        program: Option<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        kind: TabKind,
    ) -> Option<TabId> {
        let (cols, rows) = self.grid_dims();
        let cell_px = self.cell_px(ctx);
        let id = self.next_id;
        self.next_id += 1;
        let req = SpawnRequest {
            program: program.or_else(|| self.cfg.general.shell.clone()),
            args,
            cwd,
            kind,
            cols,
            rows,
            cell_px,
            scrollback: self.cfg.general.scrollback,
            term_env: self.cfg.general.term.clone(),
            integration: self.integration.clone(),
        };
        match Session::spawn(
            id,
            req,
            ctx.clone(),
            self.registry.clone(),
            self.palette.clone(),
        ) {
            Ok(s) => {
                self.sessions.push(s);
                self.activate(id);
                Some(id)
            }
            Err(e) => {
                self.toast(format!("failed to open tab: {e:#}"));
                None
            }
        }
    }

    fn tree(&self) -> TabTree {
        let guards: Vec<_> = self
            .sessions
            .iter()
            .map(|s| s.shared.state.lock())
            .collect();
        let inputs: Vec<TabInput<'_>> = self
            .sessions
            .iter()
            .zip(guards.iter())
            .map(|(s, g)| TabInput {
                id: s.id,
                kind: s.kind,
                group_override: s.group_override.as_deref(),
                state: g,
            })
            .collect();
        workspace::build_tree(&inputs, &self.home)
    }

    fn activate(&mut self, id: TabId) {
        if self.session(id).is_none() {
            return;
        }
        self.active = Some(id);
        let tree = self.tree();
        if let Some((b, g)) = tree.group_of(id) {
            self.collapsed.remove(&g.key);
            self.collapsed.remove(b.bucket.key());
        }
        if !matches!(self.mode, Mode::Normal) {
            self.leave_mode();
        }
    }

    fn close_tab(&mut self, ctx: &egui::Context, id: TabId) {
        let ordered = self.tree().ordered_tabs();
        let Some(pos) = self.sessions.iter().position(|s| s.id == id) else {
            return;
        };
        let mut session = self.sessions.remove(pos);
        tracing::info!(tab = id, pid = session.shell_pid, "closing tab");
        session.kill();
        drop(session);
        if self.active == Some(id) {
            self.active = None;
            let idx = ordered.iter().position(|t| *t == id).unwrap_or(0);
            let candidate = ordered
                .iter()
                .enumerate()
                .filter(|(_, t)| **t != id)
                .min_by_key(|(i, _)| i.abs_diff(idx))
                .map(|(_, t)| *t);
            if let Some(next) = candidate {
                self.activate(next);
            }
        }
        if self.sessions.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn poll_sessions(&mut self, ctx: &egui::Context) {
        self.drain_clipboard_paste();
        let window_focused = self.window_focused;
        let active = self.active;
        let notif = self.cfg.notifications.clone();
        let close_on_exit = self.cfg.general.close_on_exit;
        let home = self.home.clone();
        let ordered = self.tree().ordered_tabs();
        // Cloned per notification so the notify thread owns everything it needs to answer a
        // click without reaching back into the App.
        let notify_tx = self.notify_tx.clone();
        let notify_ctx = ctx.clone();
        let activation = |tab| {
            Some(notify::Activation {
                tab,
                tx: notify_tx.clone(),
                ctx: notify_ctx.clone(),
            })
        };
        let mut to_close = Vec::new();
        for s in &mut self.sessions {
            if s.poll_exit().is_some() && (s.kind == TabKind::Ephemeral || close_on_exit) {
                to_close.push(s.id);
            }
            let is_active = active == Some(s.id);
            // Assemble the notification context under the lock; format and dispatch without it.
            let (finished, bell, title, bell_ctx) = {
                let mut st = s.shared.state.lock();
                let finished = std::mem::take(&mut st.pending_finished);
                let bell = std::mem::replace(&mut st.bell_pending, false);
                let title = tab_title(&st, &s.program_label);
                let ctx = bell.then(|| notify::BellContext {
                    tab_title: title.clone(),
                    tab_index: ordered.iter().position(|t| *t == s.id).map_or(0, |i| i + 1),
                    foreground: st.foreground.as_ref().map(|f| f.comm.clone()),
                    running: st
                        .running
                        .as_ref()
                        .map(|r| (r.command_name.clone(), r.started_at.elapsed())),
                    program: s.program_label.clone(),
                    shell_pid: s.shell_pid,
                    cwd: match (&st.remote_cwd, st.cwd()) {
                        (Some((_, path)), _) => Some(path.clone()),
                        (None, Some(cwd)) => Some(shorten_home(cwd, &home)),
                        (None, None) => None,
                    },
                    remote_host: match &st.category {
                        ProcessCategory::RemoteSSH { target } => Some(target.clone()),
                        _ => None,
                    },
                    elevated: st.category == ProcessCategory::ElevatedRoot,
                    last_exit_code: st.last_exit_code,
                });
                (finished, bell, title, ctx)
            };
            if bell {
                s.bell_at = Some(Instant::now());
                if notif.enabled
                    && notif.on_bell
                    && !(is_active && window_focused)
                    && let Some(ctx) = &bell_ctx
                {
                    notify::notify_bell(ctx, notif.timeout_ms, activation(s.id));
                }
            }
            if s.bell_at
                .is_some_and(|t| t.elapsed() > Duration::from_secs(4))
            {
                s.bell_at = None;
            }
            for f in finished {
                let long_enough = f.elapsed.as_secs_f32() >= notif.threshold_secs;
                if notif.enabled && long_enough && !(is_active && window_focused) {
                    notify::notify_command_complete(
                        &title,
                        &f.command_name,
                        f.exit_code,
                        f.elapsed,
                        notif.timeout_ms,
                        activation(s.id),
                    );
                }
            }
        }
        for id in to_close {
            self.close_tab(ctx, id);
        }
    }

    /// Drain notifications the user clicked "Show session" on.
    ///
    /// The window is by definition unfocused when a notification fires, so this is also where
    /// verterm asks to be raised. On Wayland that request is a no-op — a client cannot take
    /// focus without an xdg-activation token — so the tab switch is the part that actually
    /// lands: whenever the user does come back, they are already on the session that rang.
    fn poll_notification_clicks(&mut self, ctx: &egui::Context) {
        let mut wanted = None;
        while let Ok(id) = self.notify_rx.try_recv() {
            wanted = Some(id);
        }
        if let Some(id) = wanted
            && self.session(id).is_some()
        {
            self.activate(id);
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// Keep `Term::is_focused` and DECSET 1004 focus reports in sync with reality.
    fn sync_focus(&mut self) {
        let state = (self.window_focused, self.active);
        if self.last_focus_state == Some(state) {
            return;
        }
        self.last_focus_state = Some(state);
        for s in &self.sessions {
            let want = self.window_focused && self.active == Some(s.id);
            let mut t = s.term.lock();
            if t.is_focused != want {
                t.is_focused = want;
                let report = t.mode().contains(TermMode::FOCUS_IN_OUT);
                drop(t);
                if report {
                    s.write(input::focus_event(want));
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Global chords → actions
    // -----------------------------------------------------------------------------------

    fn handle_global_keys(&mut self, ctx: &egui::Context) {
        let mut actions = Vec::new();
        let mut alt_chord_matched = false;
        {
            let super_down = &mut self.super_down;
            let keymap = &self.keymap;
            ctx.input_mut(|i| {
                if super_latch_is_stale(i.focused, &i.events) {
                    *super_down = false;
                }
                i.events.retain(|ev| {
                    if let Event::Key {
                        key,
                        physical_key,
                        pressed,
                        modifiers,
                        ..
                    } = ev
                    {
                        if matches!(key, Key::SuperLeft | Key::SuperRight) {
                            *super_down = *pressed;
                            return false;
                        }
                        if *pressed {
                            let chord = Chord::from_event(*key, *modifiers, *super_down);
                            if let Some(a) = keymap.lookup(chord) {
                                actions.push(a);
                                alt_chord_matched |= chord.alt;
                                return false;
                            }
                            if let Some(pk) = physical_key
                                && pk != key
                            {
                                let chord = Chord::from_event(*pk, *modifiers, *super_down);
                                if let Some(a) = keymap.lookup(chord) {
                                    actions.push(a);
                                    alt_chord_matched |= chord.alt;
                                    return false;
                                }
                            }
                        }
                    }
                    true
                });
            });
        }
        // egui-winit emits `Event::Text` alongside the `Event::Key` for an Alt chord — Alt does
        // not suppress it the way Ctrl does — and removing the Key above does not remove the
        // Text. Whatever has keyboard focus then receives the bare character: a stray `f` in
        // the find box for `Alt+F`, or a stray `j` at the shell prompt for `Alt+J`. A frame in
        // which an Alt chord fired an action is not a frame in which the user meant to type.
        if alt_chord_matched {
            ctx.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Text(_))));
        }
        for a in actions {
            self.perform(ctx, a);
        }
    }

    fn perform(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::ToggleRail => self.rail_visible = !self.rail_visible,
            Action::SelectTab(n) => {
                let tabs = self.tree().ordered_tabs();
                if let Some(id) = tabs.get(n as usize - 1) {
                    self.activate(*id);
                }
            }
            Action::NextTab | Action::PrevTab => {
                let tabs = self.tree().ordered_tabs();
                if tabs.is_empty() {
                    return;
                }
                let idx = self
                    .active
                    .and_then(|a| tabs.iter().position(|t| *t == a))
                    .unwrap_or(0);
                let next = if action == Action::NextTab {
                    (idx + 1) % tabs.len()
                } else {
                    (idx + tabs.len() - 1) % tabs.len()
                };
                self.activate(tabs[next]);
            }
            Action::MoveTabNextGroup | Action::MoveTabPrevGroup => {
                self.move_tab_group(action == Action::MoveTabNextGroup)
            }
            Action::ToggleGroup | Action::CollapseGroup | Action::ExpandGroup => {
                let tree = self.tree();
                let Some((b, g)) = self.active.and_then(|id| tree.group_of(id)) else {
                    return;
                };
                let (gkey, bkey) = (g.key.clone(), b.bucket.key().to_string());
                match action {
                    Action::ToggleGroup => {
                        if !self.collapsed.remove(&gkey) {
                            self.collapsed.insert(gkey);
                        }
                    }
                    Action::CollapseGroup => {
                        if !self.collapsed.insert(gkey) {
                            self.collapsed.insert(bkey); // already collapsed: fold the bucket too
                        }
                    }
                    _ => {
                        self.collapsed.remove(&gkey);
                        self.collapsed.remove(&bkey);
                    }
                }
            }
            Action::ViMode => self.toggle_vi(),
            Action::Search => {
                if !matches!(self.mode, Mode::Vi(_)) {
                    self.toggle_vi();
                }
                self.open_search(false);
            }
            Action::FindSession => {
                if matches!(self.mode, Mode::FindSession) {
                    self.leave_mode();
                } else {
                    self.leave_mode();
                    self.rail_visible = true;
                    self.mode = Mode::FindSession;
                }
            }
            Action::Hints => self.start_hints(),
            Action::AiPrompt => {
                if matches!(self.mode, Mode::Ai(_)) {
                    self.leave_mode();
                } else if self.ai.is_none() {
                    self.toast("AI is disabled ([ai] enabled = false or client failed to start)");
                } else if let Some(idx) = self.active_index() {
                    // Sampled before `leave_mode`, which clears a vi-mode selection — and
                    // before the overlay steals the pointer, which clears a mouse one.
                    let selection = self.sessions[idx]
                        .term
                        .lock()
                        .selection_to_string()
                        .filter(|t| !t.trim().is_empty());
                    self.leave_mode();
                    let s = &self.sessions[idx];
                    let anchor = {
                        let st = s.shared.state.lock();
                        st.prompt_line.filter(|_| st.osc133_seen)
                    }
                    .unwrap_or_else(|| terminal_view::cursor_row(&s.term.lock()));
                    // Only offer the retry once the command that suggestion produced has
                    // actually failed; a green last command has nothing to diagnose.
                    let failed = self.sessions[idx]
                        .shared
                        .state
                        .lock()
                        .last_command
                        .as_ref()
                        .is_some_and(|c| c.exit_code.is_some_and(|e| e != 0));
                    self.mode = Mode::Ai(Box::new(AiOverlayState::new(overlays::AiOpen {
                        anchor_row: anchor,
                        selection,
                        position: self.cfg.ai.position,
                        saved_frac: self.ui_state.ai_overlay.map(|f| (f[0], f[1])),
                        retry: failed.then(|| self.ai_last_question.clone()).flatten(),
                    })));
                }
            }
            Action::NewTab => {
                let cwd = self.active_index().and_then(|i| {
                    self.sessions[i]
                        .shared
                        .state
                        .lock()
                        .cwd()
                        .map(|p| p.to_path_buf())
                });
                self.spawn_tab(ctx, None, Vec::new(), cwd, TabKind::Normal);
            }
            Action::CloseTab => {
                if let Some(id) = self.active {
                    self.close_tab(ctx, id);
                }
            }
            Action::NewScratchpad => {
                self.leave_mode();
                self.mode = Mode::Prompt(PromptState::default());
            }
            Action::CommandPalette => {
                if matches!(self.mode, Mode::Palette(_)) {
                    self.leave_mode();
                } else {
                    self.leave_mode();
                    self.mode = Mode::Palette(PaletteState::default());
                }
            }
            Action::FontIncrease => self.font_size = (self.font_size + 1.0).min(48.0),
            Action::FontDecrease => self.font_size = (self.font_size - 1.0).max(6.0),
            Action::FontReset => self.font_size = self.cfg.font.size.clamp(6.0, 48.0),
            Action::ScrollPageUp => self.with_active_term(|t| t.scroll_display(Scroll::PageUp)),
            Action::ScrollPageDown => self.with_active_term(|t| t.scroll_display(Scroll::PageDown)),
            Action::ScrollToBottom => self.with_active_term(|t| t.scroll_display(Scroll::Bottom)),
            Action::ClearScrollback => self.with_active_term(|t| t.grid_mut().clear_history()),
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    fn with_active_term(
        &mut self,
        f: impl FnOnce(&mut alacritty_terminal::Term<crate::session::EventProxy>),
    ) {
        if let Some(idx) = self.active_index() {
            let mut t = self.sessions[idx].term.lock();
            f(&mut t);
        }
    }

    fn move_tab_group(&mut self, forward: bool) {
        let tree = self.tree();
        let keys = tree.group_keys();
        let Some(active) = self.active else { return };
        let Some((_, g)) = tree.group_of(active) else {
            return;
        };
        if keys.len() < 2 {
            self.toast("only one group exists");
            return;
        }
        let idx = keys.iter().position(|k| *k == g.key).unwrap_or(0);
        let target = if forward {
            (idx + 1) % keys.len()
        } else {
            (idx + keys.len() - 1) % keys.len()
        };
        let target_key = keys[target].clone();
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == active) else {
            return;
        };
        let natural =
            workspace::natural_group_key(session.kind, &session.shared.state.lock(), &self.home);
        session.group_override = if target_key == natural {
            None
        } else {
            Some(target_key)
        };
    }

    // -----------------------------------------------------------------------------------
    // Modes
    // -----------------------------------------------------------------------------------

    fn leave_mode(&mut self) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            // The rail shows results only while the box owns the keyboard; leaving with a
            // query still in it would strand the tree behind a filter nothing is driving.
            Mode::FindSession => self.find.clear(),
            Mode::Ai(mut st) => st.abort(),
            Mode::Vi(_) => self.with_active_term(|t| {
                if t.mode().contains(TermMode::VI) {
                    t.toggle_vi_mode();
                }
                t.selection = None;
            }),
            _ => {}
        }
    }

    /// Drive the scrollback half of the find box: debounce, fire, collect.
    ///
    /// Identity matching is a few string compares and happens inline every frame. This is the
    /// expensive half — it takes every session's `Term` lock — so it runs on a worker thread
    /// (hard rule 1), only once the query has stopped moving, and only for a query long enough
    /// to mean something.
    fn poll_find(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.find.rx
            && let Ok((generation, hits)) = rx.try_recv()
        {
            self.find.rx = None;
            // A slow pass for an older query must never replace a newer answer.
            if generation >= self.find.answered {
                self.find.answered = generation;
                self.find.content = hits;
            }
        }
        let q = self.find.query.trim().to_lowercase();
        if q.chars().count() < tabsearch::MIN_CONTENT_QUERY {
            self.find.content.clear();
            self.find.changed_at = None;
            return;
        }
        let Some(changed) = self.find.changed_at else {
            return;
        };
        if changed.elapsed() < FIND_DEBOUNCE {
            // Come back when the debounce is up even if nothing else asks for a frame.
            ctx.request_repaint_after(FIND_DEBOUNCE);
            return;
        }
        self.find.changed_at = None;
        self.find.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.find.cancel = cancel.clone();
        self.find.generation += 1;
        let search = tabsearch::ContentSearch {
            generation: self.find.generation,
            query: q,
            tabs: self
                .sessions
                .iter()
                .map(|s| (s.id, s.term.clone()))
                .collect(),
        };
        tracing::debug!(
            generation = self.find.generation,
            tabs = search.tabs.len(),
            "content search"
        );
        self.find.rx = Some(tabsearch::spawn(search, cancel, ctx.clone()));
    }

    /// Everything about a session someone might type to find it again.
    fn session_fields(&self, s: &Session, index: usize) -> tabsearch::SessionFields {
        let st = s.shared.state.lock();
        let (host, kind) = match &st.category {
            ProcessCategory::RemoteSSH { target } => (target.clone(), "ssh remote"),
            ProcessCategory::Container { name } => (name.clone(), "container box"),
            ProcessCategory::ElevatedRoot => (String::new(), "root elevated sudo"),
            ProcessCategory::Local if s.kind == TabKind::Ephemeral => {
                (String::new(), "scratch ephemeral")
            }
            ProcessCategory::Local => (String::new(), "local"),
        };
        tabsearch::SessionFields {
            index,
            title: tab_title(&st, &s.program_label),
            cwd: st
                .cwd()
                .map(|c| shorten_home(c, &self.home))
                .unwrap_or_default(),
            program: s.program_label.clone(),
            foreground: st
                .foreground
                .as_ref()
                .map(|f| f.comm.clone())
                .unwrap_or_default(),
            group: workspace::group_name_from_key(&workspace::natural_group_key(
                s.kind, &st, &self.home,
            )),
            host,
            branch: st
                .git
                .as_ref()
                .map(|g| g.branch.clone())
                .unwrap_or_default(),
            kind,
        }
    }

    fn toggle_vi(&mut self) {
        if matches!(self.mode, Mode::Vi(_)) {
            self.leave_mode();
            return;
        }
        if self.active_index().is_none() {
            return;
        }
        self.leave_mode();
        self.with_active_term(|t| {
            if !t.mode().contains(TermMode::VI) {
                t.toggle_vi_mode();
            }
        });
        self.mode = Mode::Vi(Box::default());
    }

    fn open_search(&mut self, backwards: bool) {
        let Some(idx) = self.active_index() else {
            return;
        };
        let origin = self.sessions[idx].term.lock().vi_mode_cursor.point;
        if let Mode::Vi(v) = &mut self.mode {
            v.search = Some(SearchState {
                query: String::new(),
                backwards,
                origin,
                regex: None,
                current: None,
                input_active: true,
            });
        }
    }

    /// Run the current search. `from_origin` = incremental search while typing.
    fn run_search(&mut self, from_origin: bool, reverse: bool) {
        let Some(idx) = self.active_index() else {
            return;
        };
        let term = self.sessions[idx].term.clone();
        let Mode::Vi(v) = &mut self.mode else { return };
        let Some(s) = &mut v.search else { return };
        if s.regex.is_none() || from_origin {
            s.regex = if s.query.is_empty() {
                None
            } else {
                RegexSearch::new(&regex::escape(&s.query))
                    .ok()
                    .or_else(|| RegexSearch::new(&s.query).ok())
            };
        }
        let Some(re) = s.regex.as_mut() else {
            s.current = None;
            return;
        };
        let mut t = term.lock();
        let forward = s.backwards == reverse;
        let direction = if forward {
            Direction::Right
        } else {
            Direction::Left
        };
        let start = if from_origin {
            s.origin
        } else {
            let cur = t.vi_mode_cursor.point;
            if forward {
                cur.add(&*t, Boundary::None, 1)
            } else {
                cur.sub(&*t, Boundary::None, 1)
            }
        };
        match t.search_next(re, start, direction, Side::Left, None) {
            Some(m) => {
                t.vi_goto_point(*m.start());
                s.current = Some(m);
            }
            None => s.current = None,
        }
    }

    fn start_hints(&mut self) {
        let Some(idx) = self.active_index() else {
            return;
        };
        let mut matches = {
            let t = self.sessions[idx].term.lock();
            let rows = terminal_view::viewport_rows(&t);
            let hyper = &self.cfg.hyperlinks;
            // Links come from `viewport_links`, not from the URL pass inside `find_hints`:
            // that is the one that knows about OSC 8 (whose label is whatever the program
            // chose, so no pattern can find it) and about wrapped rows. `find_hints` still
            // gets the matcher, because a URL claiming its span first is what stops the path
            // and hash patterns carving a piece out of the middle of one.
            let mut matches: Vec<HintMatch> = if hyper.enabled {
                terminal_view::viewport_links(&t, &self.link_matcher, hyper.detect)
                    .into_iter()
                    .map(|l| {
                        let head = l.head();
                        HintMatch {
                            row: head.row,
                            col_start: head.col_start,
                            col_end: head.col_end,
                            text: l.text,
                            kind: HintKind::Url,
                            uri: Some(l.uri),
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            for h in hints::find_hints(&rows, &self.link_matcher) {
                let covered = matches
                    .iter()
                    .any(|m| m.row == h.row && h.col_start < m.col_end && h.col_end > m.col_start);
                if covered || (hyper.enabled && h.kind == HintKind::Url) {
                    continue;
                }
                matches.push(h);
            }
            matches.sort_by_key(|h| (h.row, h.col_start));
            matches
        };
        matches.dedup_by(|a, b| a.row == b.row && a.col_start == b.col_start);
        if matches.is_empty() {
            self.toast("no URLs, paths, IPs, UUIDs or hashes on screen");
            return;
        }
        let tags = hints::assign_tags(matches.len());
        self.leave_mode();
        self.mode = Mode::Hints(HintsState {
            matches,
            tags,
            typed: String::new(),
        });
    }

    fn hint_action(&mut self, ctx: &egui::Context, idx: usize, hm: &HintMatch, open: bool) {
        match (open, hm.kind) {
            (false, _) => {
                ctx.copy_text(hm.text.clone());
                self.toast(format!("copied {}", hm.text));
            }
            (true, HintKind::Url) => {
                let uri = hm.uri.clone().unwrap_or_else(|| hm.text.clone());
                self.open_uri(&uri);
            }
            (true, _) => {
                let text = shell_quote(&hm.text);
                self.sessions[idx].paste(&text);
            }
        }
    }

    /// Hand a URI to the configured opener.
    ///
    /// The allowlist is re-checked here rather than trusted from whoever found the link: OSC 8
    /// URIs come straight out of program output and never passed through detection, and this
    /// is the single point where a URI leaves verterm for a desktop handler.
    fn open_uri(&mut self, uri: &str) {
        if !self.link_matcher.allows(uri) {
            let scheme = uri.split(':').next().unwrap_or(uri);
            self.toast(format!(
                "{scheme}: links are not enabled — add it to [hyperlinks].schemes"
            ));
            return;
        }
        let Some((program, args)) = self.cfg.hyperlinks.opener.split_first() else {
            self.toast("[hyperlinks].opener is empty");
            return;
        };
        match std::process::Command::new(program)
            .args(args)
            .arg(uri)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => self.toast(format!("opened {}", elide_menu_text(uri))),
            Err(e) => self.toast(format!("{program} failed: {e}")),
        }
    }

    /// Refresh `self.links` for the active tab, but only when something it depends on moved.
    ///
    /// A repaint is not a grid change — cursor blink alone asks for two a second on an idle
    /// tab — so the key is the reader thread's grid generation plus the scroll position and
    /// size, which together are the whole input to the scan.
    fn sync_links(&mut self, idx: usize, cols: u16, rows: u16) {
        if !self.cfg.hyperlinks.enabled {
            if !self.links.is_empty() {
                self.links.clear();
                self.links_key = None;
            }
            return;
        }
        let s = &self.sessions[idx];
        let generation = s.shared.grid_generation();
        let offset = s.term.lock().grid().display_offset();
        let key = (s.id, generation, offset, cols, rows);
        if self.links_key == Some(key) {
            return;
        }
        let detect = self.cfg.hyperlinks.detect;
        self.links = terminal_view::viewport_links(&s.term.lock(), &self.link_matcher, detect);
        self.links_key = Some(key);
        self.hovered_link = None;
    }

    // -----------------------------------------------------------------------------------
    // Terminal input
    // -----------------------------------------------------------------------------------

    fn handle_terminal_input(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        term_rect: Rect,
        response: &egui::Response,
    ) {
        if self.mode.text_input_active() {
            return;
        }
        // While a context menu is up it owns the pointer: draining events here would starve
        // its buttons, and a right-click would also start a selection underneath it.
        if egui::Popup::is_any_open(ctx) {
            return;
        }
        let events = ctx.input_mut(|i| std::mem::take(&mut i.events));
        let mods = ctx.input(|i| i.modifiers);
        let hover = response.hovered()
            || ctx
                .input(|i| i.pointer.hover_pos())
                .is_some_and(|p| term_rect.contains(p));
        match &self.mode {
            Mode::Normal => self.normal_input(ctx, idx, term_rect, events, mods, hover),
            Mode::Vi(_) => self.vi_input(ctx, idx, events, mods, hover),
            Mode::Hints(_) => self.hints_input(ctx, idx, events),
            _ => {}
        }
    }

    fn wheel_lines(&mut self, unit: egui::MouseWheelUnit, delta: egui::Vec2, rows: usize) -> i32 {
        let lines = match unit {
            egui::MouseWheelUnit::Line => delta.y * 3.0,
            egui::MouseWheelUnit::Point => delta.y / self.metrics.h,
            egui::MouseWheelUnit::Page => delta.y * rows as f32,
        };
        self.scroll_accum += lines;
        let whole = self.scroll_accum.trunc();
        self.scroll_accum -= whole;
        whole as i32
    }

    fn normal_input(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        term_rect: Rect,
        events: Vec<Event>,
        mods: Modifiers,
        hover: bool,
    ) {
        let term = self.sessions[idx].term.clone();
        let mode_flags = *term.lock().mode();
        let rows = self.sessions[idx].grid_size().1 as usize;
        let mut bytes: Vec<u8> = Vec::new();
        let mut copy_selection = false;
        let mut paste_text: Option<String> = None;
        let mut scroll = 0i32;
        let mut open_link: Option<String> = None;
        let activate = if self.cfg.hyperlinks.enabled {
            self.cfg.hyperlinks.activate
        } else {
            LinkActivate::None
        };

        for ev in events {
            match ev {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if let Some(b) = input::encode_key(key, modifiers, &mode_flags) {
                        bytes.extend(b);
                    }
                }
                Event::Text(t) => bytes.extend(input::encode_text(&t, mods.alt)),
                Event::Ime(egui::ImeEvent::Commit(t)) => bytes.extend(t.as_bytes()),
                Event::Paste(t) => {
                    if mods.shift {
                        paste_text = Some(t);
                    } else {
                        bytes.push(0x16);
                    }
                }
                Event::Copy => {
                    if mods.shift {
                        copy_selection = true;
                    } else {
                        bytes.push(0x03);
                    }
                }
                Event::Cut => {
                    if !mods.shift {
                        bytes.push(0x18);
                    }
                }
                Event::MouseWheel { unit, delta, .. } if hover => {
                    scroll += self.wheel_lines(unit, delta, rows)
                }
                Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers,
                } => {
                    // A link is armed on press and fired on release, and only if the release
                    // lands on the same link with nothing selected. Firing on press would
                    // make it impossible to start a selection inside a URL, and would fire
                    // mid-drag; requiring the same link is what makes a drag that happens to
                    // end on another link do nothing.
                    let hit = match activate {
                        LinkActivate::None => None,
                        LinkActivate::Click if modifiers.any() => None,
                        LinkActivate::Click => {
                            terminal_view::link_at(&self.links, pos, term_rect, self.metrics)
                        }
                        LinkActivate::CtrlClick if !modifiers.ctrl => None,
                        LinkActivate::CtrlClick => {
                            terminal_view::link_at(&self.links, pos, term_rect, self.metrics)
                        }
                    };
                    if pressed {
                        self.link_press = hit;
                    } else if let Some(armed) = self.link_press.take()
                        && hit == Some(armed)
                        && term
                            .lock()
                            .selection
                            .as_ref()
                            .is_none_or(|sel| sel.is_empty())
                    {
                        open_link = self.links.get(armed).map(|l| l.uri.clone());
                    }
                    if pressed && term_rect.contains(pos) {
                        let now = Instant::now();
                        let clicks = match self.last_click {
                            Some((t, p, n))
                                if now.duration_since(t) < Duration::from_millis(350)
                                    && p.distance(pos) < 4.0 =>
                            {
                                n % 3 + 1
                            }
                            _ => 1,
                        };
                        self.last_click = Some((now, pos, clicks));
                        let ty = match clicks {
                            2 => SelectionType::Semantic,
                            3 => SelectionType::Lines,
                            _ => SelectionType::Simple,
                        };
                        let mut t = term.lock();
                        let (point, side) =
                            terminal_view::pos_to_point(pos, term_rect, self.metrics, &t);
                        t.selection = Some(Selection::new(ty, point, side));
                        self.selection_drag = true;
                    } else if !pressed && self.selection_drag {
                        self.selection_drag = false;
                        let mut t = term.lock();
                        if t.selection.as_ref().is_some_and(|s| s.is_empty()) {
                            t.selection = None;
                        }
                    }
                }
                Event::PointerMoved(pos) if self.selection_drag => {
                    let mut t = term.lock();
                    let (point, side) =
                        terminal_view::pos_to_point(pos, term_rect, self.metrics, &t);
                    if let Some(sel) = t.selection.as_mut() {
                        sel.update(point, side);
                    }
                }
                _ => {}
            }
        }

        if let Some(uri) = open_link {
            self.open_uri(&uri);
        }
        if copy_selection && let Some(text) = term.lock().selection_to_string() {
            ctx.copy_text(text);
        }
        if let Some(t) = paste_text {
            self.sessions[idx].paste(&t);
        }
        if scroll != 0 {
            if mode_flags.contains(TermMode::ALT_SCREEN)
                && mode_flags.contains(TermMode::ALTERNATE_SCROLL)
            {
                bytes.extend(input::wheel_as_arrows(scroll, &mode_flags));
            } else {
                term.lock().scroll_display(Scroll::Delta(scroll));
            }
        }
        if !bytes.is_empty() {
            {
                let mut t = term.lock();
                if t.grid().display_offset() != 0 {
                    t.scroll_display(Scroll::Bottom);
                }
                t.selection = None;
            }
            self.sessions[idx].write(bytes);
        }
    }

    fn vi_input(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        events: Vec<Event>,
        mods: Modifiers,
        hover: bool,
    ) {
        let rows = self.sessions[idx].grid_size().1 as usize;
        for ev in events {
            match ev {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => self.vi_key(ctx, idx, key, modifiers),
                Event::Text(t) => {
                    for c in t.chars() {
                        self.vi_char(ctx, idx, c);
                    }
                }
                Event::Copy if mods.shift => self.yank(ctx, idx),
                Event::MouseWheel { unit, delta, .. } if hover => {
                    let lines = self.wheel_lines(unit, delta, rows);
                    if lines != 0 {
                        self.sessions[idx]
                            .term
                            .lock()
                            .scroll_display(Scroll::Delta(lines));
                    }
                }
                _ => {}
            }
        }
    }

    /// Right-click menu for the grid: clipboard plus whatever the click landed on. Items are
    /// collected into a `TermMenuAction` and applied after the closure, so the menu body never
    /// needs `&mut self` (and never holds the `Term` lock while egui lays widgets out).
    fn terminal_context_menu(
        &mut self,
        ctx: &egui::Context,
        idx: usize,
        term_rect: Rect,
        response: &egui::Response,
    ) {
        // Sample the grid once, as the menu opens: it stays open across frames while output
        // scrolls, and stale offsets would make "Open" act on the wrong text.
        if response.secondary_clicked() {
            let pos = ctx.input(|i| i.pointer.interact_pos());
            let term = &self.sessions[idx].term;
            let (selection, hint) = {
                let t = term.lock();
                let selection = t.selection_to_string().filter(|s| !s.is_empty());
                let hint = pos.filter(|p| term_rect.contains(*p)).and_then(|p| {
                    terminal_view::hint_at(p, term_rect, self.metrics, &t, &self.link_matcher)
                });
                (selection, hint)
            };
            let link = pos
                .filter(|_| self.cfg.hyperlinks.enabled)
                .and_then(|p| terminal_view::link_at(&self.links, p, term_rect, self.metrics))
                .and_then(|i| self.links.get(i).cloned());
            let cwd = self.sessions[idx]
                .shared
                .state
                .lock()
                .cwd()
                .map(|p| p.to_path_buf());
            self.term_menu = TermMenuContext {
                selection,
                hint,
                link,
                cwd,
            };
        }

        let menu = self.term_menu.clone();
        let mut action = None;
        response.context_menu(|ui| {
            if ui
                .add_enabled(menu.selection.is_some(), egui::Button::new("Copy"))
                .clicked()
            {
                action = Some(TermMenuAction::Copy);
                ui.close();
            }
            if ui.button("Paste").clicked() {
                action = Some(TermMenuAction::Paste);
                ui.close();
            }
            if let Some(l) = &menu.link {
                ui.separator();
                // The *target* is what the items name, never the label. An OSC 8 link can put
                // any text on screen for any URI, so a menu that offered to "Open
                // https://your-bank" while pointing somewhere else would be the terminal
                // helping with the deception.
                let target = elide_menu_text(&l.uri);
                if ui.button(format!("Open {target}")).clicked() {
                    action = Some(TermMenuAction::OpenLink);
                    ui.close();
                }
                if ui.button("Copy link address").clicked() {
                    action = Some(TermMenuAction::CopyLink);
                    ui.close();
                }
            }
            // Paths, IPs, UUIDs and hashes — anything under the pointer that is not already
            // covered by the link items above.
            if let Some(h) = &menu.hint
                && !(menu.link.is_some() && h.kind == HintKind::Url)
            {
                ui.separator();
                let label = elide_menu_text(&h.text);
                if h.kind == HintKind::Url && ui.button(format!("Open {label}")).clicked() {
                    action = Some(TermMenuAction::OpenHint);
                    ui.close();
                }
                if ui.button(format!("Copy {label}")).clicked() {
                    action = Some(TermMenuAction::CopyHint);
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Select all").clicked() {
                action = Some(TermMenuAction::SelectAll);
                ui.close();
            }
            if ui.button("Search…").clicked() {
                action = Some(TermMenuAction::Run(Action::Search));
                ui.close();
            }
            if ui.button("Hints").clicked() {
                action = Some(TermMenuAction::Run(Action::Hints));
                ui.close();
            }
            let ask = if menu.selection.is_some() {
                "Ask AI about selection"
            } else {
                "Ask AI"
            };
            if ui.button(ask).clicked() {
                action = Some(TermMenuAction::Run(Action::AiPrompt));
                ui.close();
            }
            ui.separator();
            if ui
                .add_enabled(menu.cwd.is_some(), egui::Button::new("New tab here"))
                .clicked()
            {
                action = Some(TermMenuAction::NewTabHere);
                ui.close();
            }
            if ui.button("Clear scrollback").clicked() {
                action = Some(TermMenuAction::Run(Action::ClearScrollback));
                ui.close();
            }
            if ui.button("Close tab").clicked() {
                action = Some(TermMenuAction::Run(Action::CloseTab));
                ui.close();
            }
        });

        let Some(action) = action else { return };
        match action {
            TermMenuAction::Copy => {
                if let Some(text) = menu.selection {
                    let n = text.lines().count();
                    ctx.copy_text(text);
                    self.toast(format!("copied {n} line{}", if n == 1 { "" } else { "s" }));
                }
            }
            TermMenuAction::Paste => self.request_clipboard_paste(ctx, idx),
            TermMenuAction::OpenLink => {
                if let Some(l) = menu.link {
                    self.open_uri(&l.uri);
                }
            }
            TermMenuAction::CopyLink => {
                if let Some(l) = menu.link {
                    self.toast(format!("copied {}", elide_menu_text(&l.uri)));
                    ctx.copy_text(l.uri);
                }
            }
            TermMenuAction::OpenHint => {
                if let Some(h) = menu.hint {
                    self.hint_action(ctx, idx, &h, true);
                }
            }
            TermMenuAction::CopyHint => {
                if let Some(h) = menu.hint {
                    self.hint_action(ctx, idx, &h, false);
                }
            }
            TermMenuAction::SelectAll => {
                let mut t = self.sessions[idx].term.lock();
                t.selection = terminal_view::select_viewport(&mut t);
            }
            TermMenuAction::NewTabHere => {
                self.spawn_tab(ctx, None, Vec::new(), menu.cwd, TabKind::Normal);
            }
            TermMenuAction::Run(a) => self.perform(ctx, a),
        }
    }

    /// Read the clipboard on a helper thread and paste it next frame. Clipboard access talks
    /// to the compositor and can block, so it never happens on the GUI thread (hard rule 1);
    /// the text comes back through a channel and goes out via the normal bracketed-paste path.
    fn request_clipboard_paste(&mut self, ctx: &egui::Context, idx: usize) {
        let id = self.sessions[idx].id;
        let (tx, rx) = std::sync::mpsc::channel();
        self.paste_rx = Some(rx);
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    Ok(text) if !text.is_empty() => {
                        let _ = tx.send((id, text));
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("clipboard read failed: {e}"),
                }
                ctx.request_repaint();
            });
        if spawned.is_err() {
            self.toast("could not read the clipboard");
        }
    }

    /// Deliver a clipboard read requested by the context menu.
    fn drain_clipboard_paste(&mut self) {
        let Some(rx) = &self.paste_rx else { return };
        let Ok((id, text)) = rx.try_recv() else {
            return;
        };
        self.paste_rx = None;
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            s.paste(&text);
        }
    }

    fn yank(&mut self, ctx: &egui::Context, idx: usize) {
        let text = {
            let mut t = self.sessions[idx].term.lock();
            let text = t.selection_to_string();
            t.selection = None;
            text
        };
        match text {
            Some(text) if !text.is_empty() => {
                let n = text.lines().count();
                ctx.copy_text(text);
                self.toast(format!("yanked {n} line{}", if n == 1 { "" } else { "s" }));
            }
            _ => {}
        }
    }

    fn vi_motion(&mut self, idx: usize, motion: ViMotion) {
        self.sessions[idx].term.lock().vi_motion(motion);
    }

    fn vi_scroll(&mut self, idx: usize, scroll: Scroll) {
        self.sessions[idx].term.lock().scroll_display(scroll);
    }

    fn vi_toggle_selection(&mut self, idx: usize, ty: SelectionType) {
        let mut t = self.sessions[idx].term.lock();
        let point = t.vi_mode_cursor.point;
        match &t.selection {
            Some(s) if s.ty == ty => t.selection = None,
            _ => {
                let mut s = Selection::new(ty, point, Side::Left);
                s.update(point, Side::Right);
                t.selection = Some(s);
            }
        }
    }

    fn vi_key(&mut self, ctx: &egui::Context, idx: usize, key: Key, m: Modifiers) {
        let rows = self.sessions[idx].grid_size().1 as i32;
        match key {
            Key::Escape => {
                let had_selection = {
                    let mut t = self.sessions[idx].term.lock();
                    let had = t.selection.is_some();
                    t.selection = None;
                    had
                };
                if !had_selection {
                    self.leave_mode();
                }
            }
            Key::Enter => self.yank(ctx, idx),
            Key::ArrowUp => self.vi_motion(idx, ViMotion::Up),
            Key::ArrowDown => self.vi_motion(idx, ViMotion::Down),
            Key::ArrowLeft => self.vi_motion(idx, ViMotion::Left),
            Key::ArrowRight => self.vi_motion(idx, ViMotion::Right),
            Key::Home => self.vi_motion(idx, ViMotion::First),
            Key::End => self.vi_motion(idx, ViMotion::Last),
            Key::PageUp => self.vi_scroll(idx, Scroll::PageUp),
            Key::PageDown => self.vi_scroll(idx, Scroll::PageDown),
            Key::B if m.ctrl => self.vi_scroll(idx, Scroll::PageUp),
            Key::F if m.ctrl => self.vi_scroll(idx, Scroll::PageDown),
            Key::U if m.ctrl => self.vi_scroll(idx, Scroll::Delta(rows / 2)),
            Key::D if m.ctrl => self.vi_scroll(idx, Scroll::Delta(-rows / 2)),
            Key::Y if m.ctrl => self.vi_scroll(idx, Scroll::Delta(1)),
            Key::E if m.ctrl => self.vi_scroll(idx, Scroll::Delta(-1)),
            Key::V if m.ctrl => self.vi_toggle_selection(idx, SelectionType::Block),
            _ => {}
        }
    }

    fn vi_char(&mut self, ctx: &egui::Context, idx: usize, c: char) {
        use ViMotion::*;
        let motion = match c {
            'h' => Some(Left),
            'j' => Some(Down),
            'k' => Some(Up),
            'l' => Some(Right),
            '0' => Some(First),
            '$' => Some(Last),
            '^' => Some(FirstOccupied),
            'H' => Some(High),
            'M' => Some(Middle),
            'L' => Some(Low),
            'b' => Some(SemanticLeft),
            'w' => Some(SemanticRight),
            'e' => Some(SemanticRightEnd),
            'B' => Some(WordLeft),
            'W' => Some(WordRight),
            'E' => Some(WordRightEnd),
            '%' => Some(Bracket),
            '{' => Some(ParagraphUp),
            '}' => Some(ParagraphDown),
            _ => None,
        };
        if let Some(mo) = motion {
            self.vi_motion(idx, mo);
            return;
        }
        match c {
            'g' => self.vi_scroll(idx, Scroll::Top),
            'G' => self.vi_scroll(idx, Scroll::Bottom),
            'v' => self.vi_toggle_selection(idx, SelectionType::Simple),
            'V' => self.vi_toggle_selection(idx, SelectionType::Lines),
            'y' => self.yank(ctx, idx),
            '/' => self.open_search(false),
            '?' => self.open_search(true),
            'n' => self.run_search(false, false),
            'N' => self.run_search(false, true),
            'i' | 'q' => self.leave_mode(),
            _ => {}
        }
    }

    fn hints_input(&mut self, ctx: &egui::Context, idx: usize, events: Vec<Event>) {
        let mut exit = false;
        let mut chosen: Option<(HintMatch, bool)> = None;
        if let Mode::Hints(h) = &mut self.mode {
            'outer: for ev in events {
                match ev {
                    Event::Key {
                        key: Key::Escape,
                        pressed: true,
                        ..
                    } => {
                        exit = true;
                        break;
                    }
                    Event::Key {
                        key: Key::Backspace,
                        pressed: true,
                        ..
                    } => {
                        h.typed.pop();
                    }
                    Event::Text(t) => {
                        for c in t.chars() {
                            if !c.is_ascii_alphabetic() {
                                continue;
                            }
                            let open = c.is_ascii_uppercase();
                            h.typed.push(c.to_ascii_lowercase());
                            if let Some(i) = h.tags.iter().position(|tag| *tag == h.typed) {
                                chosen = Some((h.matches[i].clone(), open));
                                break 'outer;
                            }
                            if !h.tags.iter().any(|tag| tag.starts_with(&h.typed)) {
                                h.typed.clear();
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some((hm, open)) = chosen {
            self.mode = Mode::Normal;
            self.hint_action(ctx, idx, &hm, open);
        } else if exit {
            self.mode = Mode::Normal;
        }
    }

    // -----------------------------------------------------------------------------------
    // Frame
    // -----------------------------------------------------------------------------------

    /// The chrome type scale, derived from the terminal font size so zoom moves both.
    fn typography(&self) -> Typography {
        Typography::new(&self.term_fonts, (self.font_size - 0.5).clamp(9.0, 20.0))
    }

    /// Rows for an active query: sessions matched by identity first, then sessions matched by
    /// what their scrollback contains.
    ///
    /// These reuse `Row::Bucket` and `Row::Tab`, so the result list is painted, clicked,
    /// middle-clicked and right-clicked by exactly the code that draws the tree — a search view
    /// that rendered its own rows would be a second place for every rail behaviour to drift.
    fn find_rows(&self, ordered: &[TabId]) -> Vec<Row> {
        let q = self.find.query.trim().to_lowercase();
        let index_of = |id: TabId| {
            ordered
                .iter()
                .position(|t| *t == id)
                .map(|p| p + 1)
                .unwrap_or(0)
        };

        let mut identity: Vec<(tabsearch::Score, &Session)> = self
            .sessions
            .iter()
            .filter_map(|s| {
                let fields = self.session_fields(s, index_of(s.id));
                tabsearch::identity_score(&q, &fields).map(|score| (score, s))
            })
            .collect();
        identity.sort_by_key(|(score, s)| (*score, index_of(s.id)));

        // A session already listed by identity is not repeated under the content heading; the
        // first list is the stronger answer and a duplicate row would just cost a click.
        let named: HashSet<TabId> = identity.iter().map(|(_, s)| s.id).collect();
        let content: Vec<(&tabsearch::ContentHit, &Session)> = self
            .find
            .content
            .iter()
            .filter(|h| !named.contains(&h.id))
            .filter_map(|h| self.session(h.id).map(|s| (h, s)))
            .collect();

        let mut rows = Vec::new();
        // The selected result is drawn with the active-row treatment, so keyboard selection
        // uses the same visual language as the active tab instead of inventing a second one.
        let mut nth = 0usize;
        let mut mark = |mut row: TabRow| -> Row {
            row.active = nth == self.find.selected;
            nth += 1;
            Row::Tab(row)
        };
        if !identity.is_empty() {
            rows.push(Row::Bucket {
                key: "find:sessions".into(),
                label: "SESSIONS",
                icon: chrome::Icon::Folder,
                collapsed: false,
                count: identity.len(),
                elevated: false,
                tint: self.colors.accent,
            });
            for (_, s) in identity {
                rows.push(mark(self.tab_row(s, index_of(s.id))));
            }
        }
        if !content.is_empty() {
            rows.push(Row::Bucket {
                key: "find:content".into(),
                label: "CONTAINS TEXT",
                icon: chrome::Icon::Bolt,
                collapsed: false,
                count: content.len(),
                elevated: false,
                tint: self.colors.gold,
            });
            for (hit, s) in content {
                let mut row = self.tab_row(s, index_of(s.id));
                // The matching line replaces the footprint: when you searched for text, the
                // text you found is what tells you this is the right session.
                row.subtitle = Some(hit.preview.clone());
                row.load = None;
                rows.push(mark(row));
            }
        }
        rows
    }

    fn build_rows(&self, tree: &TabTree, ordered: &[TabId]) -> Vec<Row> {
        let icon_for = |key: &str| group_icon(key);
        let mut rows = Vec::new();
        for b in &tree.buckets {
            let bkey = b.bucket.key().to_string();
            let collapsed_b = self.collapsed.contains(&bkey);
            let elevated = b.bucket == Bucket::Elevated;
            rows.push(Row::Bucket {
                key: bkey,
                label: b.bucket.label(),
                icon: bucket_icon(b.bucket),
                collapsed: collapsed_b,
                count: b.groups.iter().map(|g| g.tabs.len()).sum(),
                elevated,
                tint: bucket_tint(b.bucket, &self.colors),
            });
            if collapsed_b {
                continue;
            }
            for g in &b.groups {
                let collapsed_g = self.collapsed.contains(&g.key);
                rows.push(Row::Group {
                    key: g.key.clone(),
                    name: g.name.clone(),
                    icon: icon_for(&g.key),
                    collapsed: collapsed_g,
                    count: g.tabs.len(),
                    elevated,
                });
                if collapsed_g {
                    continue;
                }
                for id in &g.tabs {
                    if let Some(s) = self.session(*id) {
                        let index = ordered
                            .iter()
                            .position(|t| t == id)
                            .map(|p| p + 1)
                            .unwrap_or(0);
                        rows.push(Row::Tab(self.tab_row(s, index)));
                    }
                }
            }
        }
        rows
    }

    fn tab_row(&self, s: &Session, index: usize) -> TabRow {
        let st = s.shared.state.lock();
        let active = self.active == Some(s.id);
        let indicator = if st.shell_exited.is_some() {
            Indicator::Exited
        } else if let Some(d) = st.running_for() {
            Indicator::Running { since: d }
        } else {
            match st.last_exit_code {
                Some(0) => Indicator::Success,
                Some(c) => Indicator::Failure(c),
                None => Indicator::Idle,
            }
        };
        let mut badges = Vec::new();
        match &st.category {
            ProcessCategory::RemoteSSH { target } => {
                badges.push(Badge::new(format!("ssh {target}"), self.colors.gold))
            }
            ProcessCategory::Container { name } => {
                badges.push(Badge::new(format!("box {name}"), self.colors.blue))
            }
            ProcessCategory::ElevatedRoot => badges.push(Badge::new("root", self.colors.vermilion)),
            ProcessCategory::Local => {}
        }
        let git = st.git.as_ref().map(|g| {
            Badge::new(
                format!("{}{}", g.branch, if g.dirty { "*" } else { "" }),
                self.colors.blue,
            )
        });
        // The second line is noise on idle background tabs; show it where it means something.
        let subtitle = if active || st.running_for().is_some() {
            Some(format_footprint(&st, &s.program_label))
        } else {
            None
        };
        // Fraction of the whole machine. The denominator is fixed, so the bar is monotonic
        // in CPU: dividing by the cores the tree *happens* to span made it collapse every
        // time a process crossed a 100% boundary.
        let load = (st.cpu_pct >= 1.0).then(|| {
            (
                cpu_load_fraction(st.cpu_pct, st.cpu_count),
                format_cpu(st.cpu_pct),
            )
        });
        // Derived from the tab's *natural* group key, not the override: the mark describes the
        // session, the group it is filed under describes where the user put it.
        let icon = group_icon(&workspace::natural_group_key(s.kind, &st, &self.home));
        let icon_color = icon_tint(&st.category, s.kind);
        TabRow {
            id: s.id,
            index,
            title: tab_title(&st, &s.program_label),
            icon,
            icon_color: match icon_color {
                IconTint::Muted => self.colors.muted,
                IconTint::Gold => self.colors.gold,
                IconTint::Blue => self.colors.blue,
                IconTint::Vermilion => self.colors.vermilion,
                IconTint::Amber => self.colors.amber,
            },
            subtitle,
            load,
            indicator,
            badges,
            git,
            elevated: matches!(st.category, ProcessCategory::ElevatedRoot),
            active,
            bell: s.bell_at.is_some(),
        }
    }

    fn central(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, time: f64) {
        let avail = ui.available_rect_before_wrap();
        let m = self.metrics;
        let ty = self.typography();
        let status_h = if self.cfg.general.status_bar {
            (m.h + 14.0).round()
        } else {
            0.0
        };
        // The grid lives in a card: margin to the window edge, padding to the glyphs.
        const OUTER: f32 = 8.0;
        const INNER: f32 = 10.0;
        let card = Rect::from_min_max(
            pos2(avail.left() + OUTER, avail.top() + OUTER),
            pos2(avail.right() - OUTER, avail.bottom() - status_h - OUTER),
        );
        let pane = card.shrink(INNER);
        let cols = ((pane.width() / m.w).floor() as u16).max(2);
        let rows = ((pane.height() / m.h).floor() as u16).max(1);
        let term_rect = Rect::from_min_size(pane.min, vec2(cols as f32 * m.w, rows as f32 * m.h));
        self.term_rect = term_rect;

        let radius = CornerRadius::same(chrome::R_CARD);
        ui.painter().rect_filled(card, radius, self.colors.bg);

        let Some(idx) = self.active_index() else {
            chrome::outline(
                ui.painter(),
                card,
                chrome::R_CARD,
                Stroke::new(1.0, self.colors.border),
            );
            ui.painter().text(
                card.center(),
                Align2::CENTER_CENTER,
                "no tabs — Ctrl+Shift+T",
                ty.ui.clone(),
                self.colors.muted,
            );
            return;
        };
        let cell_px = self.cell_px(ctx);
        self.sessions[idx].resize(cols, rows, cell_px);

        // Links are resolved before input so a click can act on the same set the frame draws.
        self.sync_links(idx, cols, rows);
        self.hovered_link = ctx
            .input(|i| i.pointer.hover_pos())
            .and_then(|p| terminal_view::link_at(&self.links, p, term_rect, self.metrics));
        if self.hovered_link.is_some()
            && self.cfg.hyperlinks.activate != LinkActivate::None
            && matches!(self.mode, Mode::Normal)
        {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let response = ui.allocate_rect(pane, Sense::click_and_drag());
        self.handle_terminal_input(ctx, idx, term_rect, &response);
        self.terminal_context_menu(ctx, idx, term_rect, &response);

        let (category, blink_style) = {
            let s = &self.sessions[idx];
            let category = s.shared.state.lock().category.clone();
            let blinking = s.term.lock().cursor_style().blinking;
            (category, blinking)
        };
        let focused = self.window_focused;
        let blink_active = self.cfg.general.cursor_blink
            && blink_style
            && focused
            && matches!(self.mode, Mode::Normal);
        let cursor_visible = !blink_active || ((time * 2.0) as u64).is_multiple_of(2);

        {
            let s = &self.sessions[idx];
            let term = s.term.lock();
            let hints_overlay = match &self.mode {
                Mode::Hints(h) => Some(HintOverlay {
                    matches: &h.matches,
                    tags: &h.tags,
                    typed: &h.typed,
                }),
                _ => None,
            };
            let search_match = match &self.mode {
                Mode::Vi(v) => v.search.as_ref().and_then(|s| s.current.as_ref()),
                _ => None,
            };
            let link_overlay = self.cfg.hyperlinks.enabled.then(|| LinkOverlay {
                links: &self.links,
                hovered: self.hovered_link,
                underline: self.cfg.hyperlinks.underline,
                color: self.cfg.hyperlinks.color,
            });
            let rc = RenderCtx {
                palette: &self.palette,
                colors: &self.colors,
                fonts: &self.term_fonts,
                font_size: self.font_size,
                metrics: m,
                focused,
                cursor_visible,
                hints: hints_overlay,
                links: link_overlay,
                search_match,
            };
            terminal_view::render(ui.painter(), term_rect, &term, &rc);

            // IME anchor so composition popups appear at the cursor.
            let cursor_vp = terminal_view::cursor_row(&term);
            let cursor_rect = Rect::from_min_size(
                pos2(term_rect.left(), term_rect.top() + cursor_vp as f32 * m.h),
                vec2(m.w, m.h),
            );
            ctx.output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect: term_rect,
                    cursor_rect,
                    should_interrupt_composition: false,
                })
            });
        }

        // The card outline doubles as the root warning — painted after the grid so it wins.
        let stroke = if category == ProcessCategory::ElevatedRoot {
            Stroke::new(2.0, self.colors.vermilion)
        } else {
            Stroke::new(1.0, self.colors.border)
        };
        chrome::outline(ui.painter(), card, chrome::R_CARD, stroke);

        if self.cfg.general.status_bar {
            let bar = Rect::from_min_max(pos2(avail.left(), avail.bottom() - status_h), avail.max);
            self.status_bar(ui, bar, idx, cols, rows, &ty);
        }
    }

    fn status_bar(
        &self,
        ui: &egui::Ui,
        bar: Rect,
        idx: usize,
        cols: u16,
        rows: u16,
        ty: &Typography,
    ) {
        let p = ui.painter().with_clip_rect(bar);
        let c = &self.colors;
        p.rect_filled(bar, CornerRadius::ZERO, c.surface);
        chrome::top_hairline(&p, bar, c.border);
        let cy = bar.center().y;
        let s = &self.sessions[idx];
        let st = s.shared.state.lock();

        // Right-hand chips first: the left side needs to know where it must stop.
        let mut right_chips = vec![Chip::plain(
            &p,
            &format!("{cols}×{rows}"),
            &ty.mono_small,
            c.faint,
        )];
        if let Some(code) = st.last_exit_code
            && code != 0
            && st.running.is_none()
        {
            right_chips.push(Chip::new(
                &p,
                &format!("exit {code}{}", notify::describe_exit(code)),
                &ty.micro,
                c.red,
            ));
        }
        if let Some(d) = st.running_for() {
            right_chips.push(Chip::new(
                &p,
                &notify::format_duration(d),
                &ty.micro,
                c.amber,
            ));
        }
        right_chips.push(Chip::plain(
            &p,
            &format_footprint(&st, &s.program_label),
            &ty.small,
            c.muted,
        ));
        let mut right = bar.right() - 12.0;
        for chip in &right_chips {
            right -= chip.width();
            chip.paint(&p, right, cy);
            right -= 10.0;
        }

        let mode_color = match self.mode {
            Mode::Normal => c.muted,
            Mode::Vi(_) => c.amber,
            Mode::Hints(_) => c.gold,
            Mode::Ai(_) => c.accent,
            _ => c.blue,
        };
        let mut x = bar.left() + 12.0;
        let mode_chip = if matches!(self.mode, Mode::Normal) {
            Chip::plain(&p, self.mode.label(), &ty.micro, mode_color)
        } else {
            Chip::new(&p, self.mode.label(), &ty.micro, mode_color)
        };
        x += mode_chip.paint(&p, x, cy) + 10.0;

        let mut chips: Vec<Chip> = Vec::new();
        match &st.category {
            ProcessCategory::RemoteSSH { target } => {
                chips.push(Chip::new(&p, &format!("ssh {target}"), &ty.micro, c.gold))
            }
            ProcessCategory::Container { name } => {
                chips.push(Chip::new(&p, &format!("box {name}"), &ty.micro, c.blue))
            }
            ProcessCategory::ElevatedRoot => {
                chips.push(Chip::new(&p, "root", &ty.micro, c.vermilion))
            }
            ProcessCategory::Local => {}
        }
        if let Some(git) = &st.git {
            chips.push(Chip::new(
                &p,
                &format!("{}{}", git.branch, if git.dirty { "*" } else { "" }),
                &ty.micro,
                c.blue,
            ));
        }
        for chip in &chips {
            x += chip.paint(&p, x, cy) + 6.0;
        }
        // A hovered link takes the cwd's place and shows its **target**, the way a browser
        // does. This is the only place the user can see where an OSC 8 link actually goes:
        // its on-screen label is chosen by the program and can say anything.
        let hovered_uri = self
            .hovered_link
            .and_then(|i| self.links.get(i))
            .map(|l| l.uri.as_str());
        let (text, color) = match hovered_uri {
            Some(uri) => (uri.to_string(), c.link),
            None => match st.cwd() {
                Some(cwd) => (shorten_home(cwd, &self.home), c.muted),
                None => (String::new(), c.muted),
            },
        };
        if !text.is_empty() {
            chrome::line(
                &p,
                x + 2.0,
                cy,
                &text,
                &ty.mono_small,
                color,
                (right - x - 12.0).max(0.0),
            );
        }
    }

    fn show_overlays(&mut self, ctx: &egui::Context) {
        let term_rect = self.term_rect;
        let screen = ctx.content_rect();
        let ty = self.typography();
        let colors = self.colors;
        let metrics = self.metrics;
        let (model, endpoint) = self
            .ai
            .as_ref()
            .map(|a| (a.model().to_string(), a.endpoint().to_string()))
            .unwrap_or_default();

        let followup = match &mut self.mode {
            Mode::Ai(state) => {
                let ai_chrome = AiChrome {
                    colors: &colors,
                    ty: &ty,
                    metrics,
                    model: &model,
                    endpoint: &endpoint,
                };
                match overlays::show_ai(ctx, term_rect, state, &ai_chrome) {
                    AiOutcome::None => Followup::None,
                    AiOutcome::Submit(q) => Followup::AiSubmit(q),
                    AiOutcome::Insert(c) => Followup::AiInsert(c),
                    AiOutcome::Execute(c) => Followup::AiExecute(c),
                    AiOutcome::Moved(f) => Followup::AiMoved(f),
                    AiOutcome::Close => Followup::CloseMode,
                }
            }
            Mode::Palette(state) => {
                match overlays::show_palette(ctx, screen, state, &self.keymap, &colors, &ty) {
                    PaletteOutcome::None => Followup::None,
                    PaletteOutcome::Run(a) => Followup::Run(a),
                    PaletteOutcome::Close => Followup::CloseMode,
                }
            }
            Mode::Prompt(state) => {
                match overlays::show_prompt(ctx, screen, "scratchpad command", state, &colors, &ty)
                {
                    PromptOutcome::None => Followup::None,
                    PromptOutcome::Submit(cmd) => Followup::SpawnScratch(cmd),
                    PromptOutcome::Cancel => Followup::CloseMode,
                }
            }
            Mode::Vi(v) => match v.search.as_mut() {
                Some(s) if s.input_active => {
                    match overlays::show_search(
                        ctx,
                        term_rect,
                        &mut s.query,
                        s.backwards,
                        s.current.is_some(),
                        &colors,
                        &ty,
                        metrics,
                    ) {
                        SearchOutcome::None => Followup::None,
                        SearchOutcome::Changed => Followup::SearchChanged,
                        SearchOutcome::Confirm => Followup::SearchConfirm,
                        SearchOutcome::Cancel => Followup::SearchCancel,
                    }
                }
                _ => Followup::None,
            },
            _ => Followup::None,
        };

        match followup {
            Followup::None => {}
            Followup::CloseMode => self.leave_mode(),
            Followup::Run(a) => {
                self.mode = Mode::Normal;
                self.perform(ctx, a);
            }
            Followup::SpawnScratch(cmd) => {
                self.mode = Mode::Normal;
                let cwd = self.active_index().and_then(|i| {
                    self.sessions[i]
                        .shared
                        .state
                        .lock()
                        .cwd()
                        .map(|p| p.to_path_buf())
                });
                let shell = self.cfg.general.shell.clone().unwrap_or_else(default_shell);
                self.spawn_tab(
                    ctx,
                    Some(shell),
                    vec!["-c".to_string(), cmd],
                    cwd,
                    TabKind::Ephemeral,
                );
            }
            Followup::AiSubmit(q) => self.ai_submit(ctx, q),
            Followup::AiMoved((x, y)) => {
                self.ui_state.ai_overlay = Some([x, y]);
                self.ui_state.save();
            }
            Followup::AiInsert(cmd) => {
                self.leave_mode();
                if let Some(idx) = self.active_index() {
                    let s = &self.sessions[idx];
                    let single = if s.term.lock().mode().contains(TermMode::BRACKETED_PASTE) {
                        cmd
                    } else {
                        cmd.replace('\n', " ")
                    };
                    s.paste(&single);
                }
            }
            Followup::AiExecute(cmd) => {
                if let Mode::Ai(st) = &self.mode {
                    self.ai_last_question = Some(st.last_sent.clone()).filter(|q| !q.is_empty());
                }
                self.leave_mode();
                if let Some(idx) = self.active_index() {
                    let s = &self.sessions[idx];
                    let single = if s.term.lock().mode().contains(TermMode::BRACKETED_PASTE) {
                        cmd
                    } else {
                        cmd.replace('\n', " ")
                    };
                    s.paste(&single);
                    s.write(b"\r".to_vec());
                }
            }
            Followup::SearchChanged => self.run_search(true, false),
            Followup::SearchConfirm => {
                if let Mode::Vi(v) = &mut self.mode
                    && let Some(s) = &mut v.search
                {
                    s.input_active = false;
                    if s.query.is_empty() {
                        v.search = None;
                    }
                }
            }
            Followup::SearchCancel => {
                let origin = match &self.mode {
                    Mode::Vi(v) => v.search.as_ref().map(|s| s.origin),
                    _ => None,
                };
                if let Some(o) = origin {
                    self.with_active_term(|t| t.vi_goto_point(o));
                }
                if let Mode::Vi(v) = &mut self.mode {
                    v.search = None;
                }
            }
        }
    }

    /// The previous command in tab `idx`, as the `[LAST]` block needs it: what ran, how it
    /// ended, and its output — located between the OSC 133;C row and the prompt row that
    /// followed it. Both are scroll-invariant line numbers, so this still finds the right
    /// text after the command scrolled the screen.
    ///
    /// `None` without shell integration: the `/proc` scanner knows a command ran but not its
    /// text, its exit code or where its output began, and a `[LAST]` block full of "unknown"
    /// would only invite the model to invent one.
    fn last_command(&self, idx: usize) -> Option<crate::ai::LastCommand> {
        let s = &self.sessions[idx];
        let (finished, from, to) = {
            let st = s.shared.state.lock();
            (st.last_command.clone()?, st.command_abs?, st.prompt_abs?)
        };
        // `133;C` is emitted from PS0 / preexec, i.e. *after* the shell echoed the newline
        // that submitted the line — so `from` is already the first output row, not the
        // command line. `133;A` comes before the new prompt is printed, so the output ends
        // one row above it. A command that printed nothing leaves `to < from`, and
        // `output_excerpt` returns an empty vec for that.
        let output = terminal_view::output_excerpt(&s.term.lock(), from, to - 1, 3, 5);
        Some(crate::ai::LastCommand {
            command: finished.command_name,
            exit_code: finished.exit_code,
            elapsed: finished.elapsed,
            output,
        })
    }

    fn ai_submit(&mut self, ctx: &egui::Context, question: String) {
        let Some(idx) = self.active_index() else {
            return;
        };
        let Some(ai) = &self.ai else { return };
        let s = &self.sessions[idx];
        let (shell, cwd, category) = {
            let st = s.shared.state.lock();
            (
                s.program_label.clone(),
                st.cwd()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                st.category.clone(),
            )
        };
        let last_lines = terminal_view::last_lines(&s.term.lock(), self.cfg.ai.context_lines);
        let selection = match &self.mode {
            Mode::Ai(st) => st.selection_payload(),
            _ => None,
        };
        let last = self.last_command(idx);
        let prompt = crate::ai::AiPrompt {
            question,
            context: crate::ai::build_context(&shell, &cwd, &last_lines),
            shell,
            cwd,
            category,
            selection,
            last,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let rx = ai.generate(prompt, cancel.clone(), ctx.clone());
        if let Mode::Ai(state) = &mut self.mode {
            state.begin_request(rx, cancel);
        }
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = match self.active_index() {
            Some(idx) => {
                let s = &self.sessions[idx];
                let st = s.shared.state.lock();
                let mut t = tab_title(&st, &s.program_label);
                if let ProcessCategory::RemoteSSH { target } = &st.category {
                    t = format!("[{target}] {t}");
                }
                if let ProcessCategory::Container { name } = &st.category {
                    t = format!("[{name}] {t}");
                }
                if st.category == ProcessCategory::ElevatedRoot {
                    t = format!("[ROOT] {t}");
                }
                format!("{t} — {}", crate::APP_NAME)
            }
            None => crate::APP_NAME.to_string(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
    }

    fn schedule_repaint(&self, ctx: &egui::Context) {
        let any_running = self
            .sessions
            .iter()
            .any(|s| s.shared.state.lock().running.is_some());
        let blink = self.cfg.general.cursor_blink && self.window_focused;
        if any_running || !self.toasts.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else if blink {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// Detect a Super latch that can no longer be real.
///
/// `super_down` is tracked by hand because `egui::Modifiers` carries no Super bit on Linux
/// (`command`/`mac_cmd` are Mac-only). The latch is set from `Key::SuperLeft/SuperRight`
/// events, so it goes stale whenever the matching *release* never arrives — which is exactly
/// what a compositor-level grab does. Summoning the window with Niri's `Super+\`` delivers the
/// release to the previously-focused surface, leaving the latch stuck on and turning every
/// later `k` into `Super+K` (the AI prompt).
///
/// Two signals prove Super is not physically held:
/// * the window is not focused — egui-winit clears its own modifier copy here too;
/// * a `ModifiersChanged` reporting no modifiers at all, or any `Text` event: holding Super
///   suppresses text composition, so a printable character can only arrive with Super up.
fn super_latch_is_stale(focused: bool, events: &[Event]) -> bool {
    if !focused {
        return true;
    }
    events.iter().any(|ev| match ev {
        Event::ModifiersChanged(m) => m.is_none(),
        Event::Text(_) => true,
        _ => false,
    })
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.window_focused = ctx.input(|i| i.focused);
        self.metrics = fonts::measure(
            &ctx,
            &self.term_fonts,
            self.font_size,
            self.cfg.font.line_height,
        );
        self.poll_config_reload(&ctx);
        self.poll_sessions(&ctx);
        self.poll_notification_clicks(&ctx);
        self.handle_global_keys(&ctx);
        self.sync_focus();
        self.toasts
            .retain(|(t, _)| t.elapsed() < Duration::from_secs(6));

        let time = ctx.input(|i| i.time);
        let tree = self.tree();
        let ordered = tree.ordered_tabs();

        if matches!(self.mode, Mode::FindSession) {
            let (esc, enter, up, down) = ctx.input(|i| {
                (
                    i.key_pressed(Key::Escape),
                    i.key_pressed(Key::Enter),
                    i.key_pressed(Key::ArrowUp),
                    i.key_pressed(Key::ArrowDown),
                )
            });
            let hits: Vec<TabId> = self
                .find_rows(&ordered)
                .into_iter()
                .filter_map(|r| match r {
                    Row::Tab(t) => Some(t.id),
                    _ => None,
                })
                .collect();
            if !hits.is_empty() {
                if down {
                    self.find.selected = (self.find.selected + 1) % hits.len();
                }
                if up {
                    self.find.selected = (self.find.selected + hits.len() - 1) % hits.len();
                }
                self.find.selected = self.find.selected.min(hits.len() - 1);
            }
            if esc {
                // First Esc clears a query, a second leaves the box — so backing out of a
                // search never also drops you out of the rail in one keystroke.
                if self.find.query.is_empty() {
                    self.leave_mode();
                } else {
                    self.find.clear();
                }
            } else if enter && let Some(id) = hits.get(self.find.selected).copied() {
                self.activate(id);
                self.find.clear();
            }
        }

        if self.rail_visible {
            self.poll_find(&ctx);
            let searching = !self.find.query.trim().is_empty();
            let rows = if searching {
                self.find_rows(&ordered)
            } else {
                self.build_rows(&tree, &ordered)
            };
            let ty = self.typography();
            let colors = self.colors;
            let frame = egui::Frame::NONE
                .fill(colors.surface)
                .inner_margin(egui::Margin::symmetric(0, 2));
            let panel = match self.cfg.general.rail_side {
                RailSide::Left => egui::Panel::left("verterm-rail"),
                RailSide::Right => egui::Panel::right("verterm-rail"),
            };
            let out = panel
                .resizable(true)
                .default_size(self.cfg.general.rail_width)
                .frame(frame)
                .show_separator_line(false)
                .show(ui, |ui| {
                    let find = rail::FindBox {
                        query: &mut self.find.query,
                        focused: matches!(self.mode, Mode::FindSession),
                        // Sessions found, not rows drawn — the category headers are rows too.
                        show_icon: self.cfg.general.show_icon,
                        results: searching
                            .then(|| rows.iter().filter(|r| matches!(r, Row::Tab(_))).count()),
                    };
                    rail::show(ui, &rows, &colors, &ty, time, find)
                })
                .inner;
            if out.focus_find {
                self.leave_mode();
                self.mode = Mode::FindSession;
            }
            if out.find_changed {
                self.find.changed_at = Some(Instant::now());
                self.find.selected = 0;
            }
            if let Some(id) = out.activate {
                self.activate(id);
                // Activating a result answers the question the box was asking.
                self.find.clear();
            }
            if let Some(key) = out.toggle_key
                && !self.collapsed.remove(&key)
            {
                self.collapsed.insert(key);
            }
            if let Some(id) = out.close {
                self.close_tab(&ctx, id);
            }
            if out.new_tab {
                self.perform(&ctx, Action::NewTab);
            }
            if out.new_scratchpad {
                self.perform(&ctx, Action::NewScratchpad);
            }
            if let Some((id, forward)) = out.move_group {
                // move_tab_group works on the active tab, so adopt the clicked one first.
                self.activate(id);
                self.move_tab_group(forward);
            }
            if let Some(id) = out.copy_cwd {
                let cwd = self
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| s.shared.state.lock().cwd().map(|p| p.to_path_buf()));
                match cwd {
                    Some(p) => {
                        let text = shorten_home(&p, &self.home);
                        ctx.copy_text(p.display().to_string());
                        self.toast(format!("copied {text}"));
                    }
                    None => self.toast("no working directory for that tab"),
                }
            }
            if out.collapse_all || out.expand_all {
                let tree = self.tree();
                if out.expand_all {
                    self.collapsed.clear();
                } else {
                    // Fold buckets as well as groups, or "collapse all" leaves half the rail open.
                    for key in tree.group_keys() {
                        self.collapsed.insert(key);
                    }
                    for b in &tree.buckets {
                        self.collapsed.insert(b.bucket.key().to_string());
                    }
                }
            }
        }

        let surface = self.colors.surface;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(surface))
            .show(ui, |ui| self.central(ui, &ctx, time));

        self.show_overlays(&ctx);
        overlays::show_toasts(
            &ctx,
            ctx.content_rect(),
            &self.toasts,
            &self.colors,
            &self.typography(),
        );
        self.update_title(&ctx);
        self.schedule_repaint(&ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.colors.surface.to_normalized_gamma_f32()
    }
}

// ---------------------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------------------

/// Is this window title just the shell's default `user@host:dir` stamp?
///
/// bash and zsh ship a `PS1` that sets the title to exactly that, so in a rail full of local
/// tabs *every* row reads `rayben@cachygram:/hom…` and the one thing that distinguishes them —
/// the directory — is the part that gets truncated away. A title in that shape tells us
/// nothing we do not already know from `cwd`, so it is skipped in favour of the basename.
/// Titles a *program* set (vim's filename, ssh's host, a build's progress) never match this
/// and still win, which is the whole reason the OSC title has precedence.
fn is_default_shell_title(t: &str) -> bool {
    let Some((user, rest)) = t.split_once('@') else {
        return false;
    };
    let host = rest.split_once(':').map(|(h, _)| h).unwrap_or(rest);
    !user.is_empty()
        && !host.is_empty()
        && !user.contains(char::is_whitespace)
        && !host.contains(char::is_whitespace)
        // A path may contain spaces, so only the part before it is checked; what matters is
        // that the title *starts* with the `user@host` stamp and nothing else precedes it.
        && !user.contains('/')
        && !host.contains('/')
}

pub fn tab_title(st: &SessionState, fallback: &str) -> String {
    if let Some(t) = &st.title {
        let t = t.trim();
        if !t.is_empty() && !is_default_shell_title(t) {
            return t.to_string();
        }
    }
    if let Some(f) = &st.foreground {
        return f.comm.clone();
    }
    if let Some((host, path)) = &st.remote_cwd {
        return format!("{host}:{path}");
    }
    if let Some(cwd) = st.cwd() {
        return cwd
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| cwd.display().to_string());
    }
    fallback.to_string()
}

pub fn human_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= K * K * K {
        format!("{:.1}G", b / (K * K * K))
    } else if b >= K * K * 10.0 {
        format!("{:.0}M", b / (K * K))
    } else if b >= K * K {
        format!("{:.1}M", b / (K * K))
    } else if b >= K {
        format!("{:.0}K", b / K)
    } else {
        format!("{bytes}B")
    }
}

/// `nvim · 42M` · `make · 8 cores · 94% · 1.2G` · `bash · 5M`
/// Left segment of the metric line: `name · memory`. The CPU reading is **not** here — it is
/// printed inside the gauge further along the row, so the two never disagree and the text keeps
/// a stable width as load moves.
pub fn format_footprint(st: &SessionState, fallback: &str) -> String {
    let name = st
        .foreground
        .as_ref()
        .map(|f| f.comm.as_str())
        .unwrap_or(fallback);
    format!("{name} · {}", human_bytes(st.rss_bytes))
}

/// The reading printed on the CPU gauge. Above one core it becomes "N.Nx" — a percentage past
/// 100 reads as a bug, and cores-worth-of-work is what the number actually means once a job
/// spans several of them.
pub fn format_cpu(cpu_pct: f32) -> String {
    if cpu_pct >= 995.0 {
        format!("{:.0}x", cpu_pct / 100.0)
    } else if cpu_pct >= 100.0 {
        format!("{:.1}x", cpu_pct / 100.0)
    } else {
        format!("{cpu_pct:.0}%")
    }
}

/// Foreground CPU as a fraction of the whole machine, for the rail's load meter.
/// `cpu_count` is 0 before the first scan lands, hence the clamp.
pub fn cpu_load_fraction(cpu_pct: f32, cpu_count: f32) -> f32 {
    (cpu_pct / 100.0 / cpu_count.max(1.0)).clamp(0.0, 1.0)
}

pub fn shorten_home(path: &std::path::Path, home: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Single-quote a string for POSIX shells when it contains anything unsafe.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@~+=,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn group_icon_matches_the_bucket_it_belongs_to() {
        assert_eq!(group_icon("remote:orohost"), chrome::Icon::Globe);
        assert_eq!(group_icon("container:sedanos"), chrome::Icon::Box);
        assert_eq!(group_icon("elevated"), chrome::Icon::Shield);
        assert_eq!(group_icon("ephemeral"), chrome::Icon::Bolt);
        assert_eq!(group_icon("local:~"), chrome::Icon::Home);
        assert_eq!(
            group_icon("local:/home/me/src/verterm"),
            chrome::Icon::Folder
        );
        // Outside $HOME with no project root is still a directory, not a home.
        assert_eq!(group_icon("local:/"), chrome::Icon::Folder);
        // Every bucket's own mark must agree with the groups it contains.
        assert_eq!(bucket_icon(Bucket::Remote), group_icon("remote:x"));
        assert_eq!(bucket_icon(Bucket::Container), group_icon("container:x"));
        assert_eq!(bucket_icon(Bucket::Elevated), group_icon("elevated"));
        assert_eq!(bucket_icon(Bucket::Ephemeral), group_icon("ephemeral"));
    }

    #[test]
    fn only_the_kinds_worth_noticing_get_a_colour() {
        assert_eq!(
            icon_tint(&ProcessCategory::Local, TabKind::Normal),
            IconTint::Muted
        );
        assert_eq!(
            icon_tint(&ProcessCategory::Local, TabKind::Ephemeral),
            IconTint::Amber
        );
        assert_eq!(
            icon_tint(&ProcessCategory::ElevatedRoot, TabKind::Normal),
            IconTint::Vermilion
        );
        assert_eq!(
            icon_tint(
                &ProcessCategory::Container { name: "c".into() },
                TabKind::Normal
            ),
            IconTint::Blue
        );
    }

    #[test]
    fn the_default_shell_title_is_skipped_so_tabs_are_told_apart() {
        // bash/zsh stamp this on every tab; it truncates to `rayben@cachygra…` in the rail and
        // the directory — the only part that differs — is what gets cut off.
        assert!(is_default_shell_title("rayben@cachygram:~/src"));
        assert!(is_default_shell_title("root@box:/etc"));
        assert!(is_default_shell_title("rayben@cachygram"));
        // Anything a program set must still win.
        assert!(!is_default_shell_title("vim: main.rs"));
        assert!(!is_default_shell_title("make -j8"));
        assert!(!is_default_shell_title("~/src/verterm"));
        assert!(!is_default_shell_title("deploy@prod running migration"));
        assert!(!is_default_shell_title("/home/me@work:notes"));
    }
    use super::*;

    #[test]
    fn super_latch_clears_when_window_unfocused() {
        assert!(super_latch_is_stale(false, &[]));
    }

    #[test]
    fn super_latch_survives_a_real_super_hold() {
        // Focused, and nothing in the queue disproves a held Super: keep the latch.
        let events = [Event::Key {
            key: Key::K,
            physical_key: Some(Key::K),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::default(),
        }];
        assert!(!super_latch_is_stale(true, &events));
    }

    #[test]
    fn super_latch_clears_on_printable_text() {
        // Typing the `k` in "docker": Super cannot be held if text composed.
        let events = [
            Event::Key {
                key: Key::K,
                physical_key: Some(Key::K),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::default(),
            },
            Event::Text("k".into()),
        ];
        assert!(super_latch_is_stale(true, &events));
    }

    #[test]
    fn super_latch_clears_when_modifiers_report_empty() {
        let events = [Event::ModifiersChanged(Modifiers::default())];
        assert!(super_latch_is_stale(true, &events));

        // A ModifiersChanged that still reports a modifier is not proof of anything.
        let held = [Event::ModifiersChanged(Modifiers {
            ctrl: true,
            ..Default::default()
        })];
        assert!(!super_latch_is_stale(true, &held));
    }

    #[test]
    fn footprint_formats() {
        let mut st = SessionState {
            rss_bytes: 42 * 1024 * 1024,
            foreground: Some(crate::session::ForegroundProc {
                comm: "nvim".into(),
            }),
            ..Default::default()
        };
        assert_eq!(format_footprint(&st, "bash"), "nvim · 42M");
        st.rss_bytes = 1_300_000_000;
        st.foreground = Some(crate::session::ForegroundProc {
            comm: "make".into(),
        });
        // The CPU reading moved into the gauge (`format_cpu`), so this segment must keep a
        // stable width as load moves — that is the point of taking it out of the text.
        for pct in [0.0, 37.0, 95.0, 105.0, 750.0] {
            st.cpu_pct = pct;
            assert_eq!(format_footprint(&st, "bash"), "make · 1.2G", "at {pct}%");
        }
        // Falls back to the shell's own name when nothing is in the foreground.
        st.foreground = None;
        assert_eq!(format_footprint(&st, "bash"), "bash · 1.2G");
    }

    #[test]
    fn cpu_reading_switches_to_cores_past_one_core() {
        assert_eq!(format_cpu(0.0), "0%");
        assert_eq!(format_cpu(37.4), "37%");
        assert_eq!(format_cpu(99.4), "99%");
        // Past a full core a percentage reads as a bug; cores-worth is what it means.
        assert_eq!(format_cpu(100.0), "1.0x");
        assert_eq!(format_cpu(320.0), "3.2x");
        // And past ten cores the decimal is noise that would widen the gauge label.
        assert_eq!(format_cpu(1600.0), "16x");
        // The label stays short enough for a 40 px gauge at every load.
        for pct in (0..=6400).step_by(7) {
            assert!(format_cpu(pct as f32).len() <= 5, "{pct}");
        }
    }

    #[test]
    fn cpu_load_fraction_is_monotonic_across_core_boundaries() {
        // The bug: the old divisor was ceil(cpu/100), so 105% normalised to 0.525 while
        // 95% normalised to 0.95 — the bar collapsed as the process got busier.
        let cores = 8.0;
        let mut prev = 0.0;
        for pct in (0..=1600).step_by(5) {
            let f = cpu_load_fraction(pct as f32, cores);
            assert!(f >= prev, "load fell at {pct}%: {f} < {prev}");
            prev = f;
        }
        assert_eq!(cpu_load_fraction(800.0, 8.0), 1.0);
        assert_eq!(cpu_load_fraction(400.0, 8.0), 0.5);
        // Saturates rather than overflowing the bar.
        assert_eq!(cpu_load_fraction(2000.0, 8.0), 1.0);
        // cpu_count is 0 until the first scan lands; must not divide by zero.
        assert_eq!(cpu_load_fraction(50.0, 0.0), 0.5);
    }

    #[test]
    fn menu_labels_are_elided_on_char_boundaries() {
        assert_eq!(elide_menu_text("short.txt"), "short.txt");
        let long = "https://example.com/".to_string() + &"a".repeat(80);
        let out = elide_menu_text(&long);
        assert_eq!(out.chars().count(), 32);
        assert!(out.ends_with('…'));
        // Multi-byte input must not panic or split a character mid-way.
        let wide = "\u{1f600}".repeat(50);
        assert_eq!(elide_menu_text(&wide).chars().count(), 32);
    }

    #[test]
    fn quoting_and_home() {
        assert_eq!(shell_quote("/etc/hosts"), "/etc/hosts");
        assert_eq!(shell_quote("my file.txt"), "'my file.txt'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        let home = std::path::Path::new("/home/me");
        assert_eq!(
            shorten_home(std::path::Path::new("/home/me/src"), home),
            "~/src"
        );
        assert_eq!(shorten_home(std::path::Path::new("/home/me"), home), "~");
        assert_eq!(shorten_home(std::path::Path::new("/srv"), home), "/srv");
    }
}

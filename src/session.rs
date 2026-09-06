//! One terminal session: a PTY, the shell living in it, the alacritty `Term` grid, and the
//! reader/writer threads that keep them decoupled from the GUI thread (zero input latency:
//! rendering never blocks I/O and I/O never blocks rendering).

use std::io::{Read, Write};
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context, Result};
use parking_lot::{FairMutex, Mutex};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::osc::{OscEvent, OscMark, OscTap};
use crate::procscan::{ScanRegistry, ScanTarget};
use crate::shell_integration::ShellIntegration;
use crate::ui::theme::Palette;

pub type TabId = u64;

// ---------------------------------------------------------------------------------------
// Shared state model (read by the UI, written by the reader thread and the /proc scanner)
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ProcessCategory {
    #[default]
    Local,
    RemoteSSH {
        target: String,
    },
    /// The shell has entered a local container (`distrobox enter <name>`, `docker`/`podman
    /// exec`, `toolbox enter`). Commands run inside that container's userland, so its distro
    /// and installed tools are not this host's — but the kernel, uid and hostname are shared.
    Container {
        name: String,
    },
    ElevatedRoot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunSource {
    /// Shell integration reported OSC 133;C / 133;D (exact timing, exact exit code).
    Osc133,
    /// Inferred from the PTY foreground process group changing (no exit code available).
    ProcScan,
}

#[derive(Clone, Debug)]
pub struct CommandRunState {
    pub command_name: String,
    pub started_at: Instant,
    pub source: RunSource,
}

#[derive(Clone, Debug)]
pub struct CommandFinished {
    pub command_name: String,
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct ForegroundProc {
    pub comm: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: String,
    pub dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabKind {
    Normal,
    /// Ad-hoc scratchpad: closes as soon as its command exits.
    Ephemeral,
}

#[derive(Default)]
pub struct SessionState {
    /// Window title set by the application (OSC 0/2).
    pub title: Option<String>,
    /// Working directory reported by OSC 7 for the local host.
    pub osc_cwd: Option<PathBuf>,
    /// Working directory + host reported by OSC 7 from a *remote* host (display only).
    pub remote_cwd: Option<(String, String)>,
    /// Working directory read from `/proc/<shell>/cwd` (authoritative when readable).
    pub proc_cwd: Option<PathBuf>,
    /// Once true, OSC 133 drives the command lifecycle and the scanner only decorates it.
    pub osc133_seen: bool,
    /// Viewport row of the last prompt start (OSC 133;A), for overlay placement.
    pub prompt_line: Option<usize>,
    /// Grid line of the last prompt start, in the scroll-invariant coordinate described on
    /// [`GridPos::abs`]. `prompt_line` is a viewport row and goes stale the moment output
    /// scrolls; this one still points at the same text.
    pub prompt_abs: Option<i64>,
    /// Grid line of the last command start (OSC 133;C), same coordinate. Together with
    /// `prompt_abs` it bounds the output of the command that just ran.
    pub command_abs: Option<i64>,
    /// The most recent finished command, kept for the AI `[LAST]` block. `pending_finished`
    /// cannot serve this: the notification path drains it every frame.
    pub last_command: Option<CommandFinished>,
    pub running: Option<CommandRunState>,
    pub last_exit_code: Option<i32>,
    pub last_finished_at: Option<Instant>,
    /// Completed commands not yet turned into notifications by the UI.
    pub pending_finished: Vec<CommandFinished>,
    pub bell_pending: bool,
    pub category: ProcessCategory,
    pub foreground: Option<ForegroundProc>,
    /// Summed CPU% over the foreground process tree (may exceed 100).
    pub cpu_pct: f32,
    /// Cores available to the tree; the denominator for `cpu_pct`. 0 until the first
    /// scan, so every reader must clamp it to at least 1.
    pub cpu_count: f32,
    pub proc_count: usize,
    pub rss_bytes: u64,
    pub project_root: Option<PathBuf>,
    pub git: Option<GitInfo>,
    /// `Some(code)` once the shell process has exited.
    pub shell_exited: Option<Option<i32>>,
}

impl SessionState {
    pub fn cwd(&self) -> Option<&Path> {
        self.proc_cwd.as_deref().or(self.osc_cwd.as_deref())
    }

    pub fn start_command(&mut self, name: Option<String>, source: RunSource) {
        match &mut self.running {
            Some(run) => {
                if run.command_name.is_empty()
                    && let Some(n) = name
                {
                    run.command_name = n;
                }
                if source == RunSource::Osc133 {
                    run.source = source;
                }
            }
            None => {
                self.running = Some(CommandRunState {
                    command_name: name.unwrap_or_default(),
                    started_at: Instant::now(),
                    source,
                });
            }
        }
    }

    pub fn finish_command(&mut self, exit_code: Option<i32>) {
        let now = Instant::now();
        if let Some(run) = self.running.take() {
            let finished = CommandFinished {
                command_name: run.command_name,
                exit_code,
                elapsed: now.duration_since(run.started_at),
            };
            self.last_command = Some(finished.clone());
            self.pending_finished.push(finished);
            self.last_exit_code = exit_code;
            self.last_finished_at = Some(now);
        } else if matches!(exit_code, Some(c) if c != 0) {
            // A failure reported without a matching start still deserves the red indicator.
            self.last_exit_code = exit_code;
            self.last_finished_at = Some(now);
        }
    }

    pub fn running_for(&self) -> Option<Duration> {
        self.running.as_ref().map(|r| r.started_at.elapsed())
    }
}

pub struct SessionShared {
    pub id: TabId,
    pub shell_pid: i32,
    pub state: Mutex<SessionState>,
}

// ---------------------------------------------------------------------------------------
// alacritty glue
// ---------------------------------------------------------------------------------------

/// Minimal `Dimensions` impl so we do not depend on alacritty's test-only `TermSize`.
#[derive(Clone, Copy, Debug)]
pub struct GridSize {
    pub cols: usize,
    pub lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Receives events emitted by `Term` while the reader thread drives the parser.
pub struct EventProxy {
    ctx: egui::Context,
    writer: Sender<Vec<u8>>,
    shared: Arc<SessionShared>,
    palette: Arc<Palette>,
    size: Arc<Mutex<WindowSize>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                self.shared.state.lock().title = Some(title);
                self.ctx.request_repaint();
            }
            Event::ResetTitle => {
                self.shared.state.lock().title = None;
                self.ctx.request_repaint();
            }
            Event::Bell => {
                self.shared.state.lock().bell_pending = true;
                self.ctx.request_repaint();
            }
            Event::PtyWrite(text) => {
                let _ = self.writer.send(text.into_bytes());
            }
            Event::ClipboardStore(_, text) => {
                // OSC 52 copy: allowed (matches alacritty's default `OnlyCopy`).
                self.ctx.copy_text(text);
            }
            Event::ClipboardLoad(_, _) => {
                // OSC 52 paste requests are denied: applications must not read the clipboard.
            }
            Event::ColorRequest(index, format) => {
                let rgb = self.palette.rgb_by_index(index);
                let _ = self.writer.send(format(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let size = *self.size.lock();
                let _ = self.writer.send(format(size).into_bytes());
            }
            Event::Wakeup | Event::CursorBlinkingChange | Event::MouseCursorDirty => {
                self.ctx.request_repaint();
            }
            Event::Exit | Event::ChildExit(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------------------

pub struct SpawnRequest {
    /// `None` → the user's login shell.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub kind: TabKind,
    pub cols: u16,
    pub rows: u16,
    pub cell_px: (u16, u16),
    pub scrollback: usize,
    pub term_env: String,
    pub integration: Option<Arc<ShellIntegration>>,
}

pub struct Session {
    pub id: TabId,
    pub kind: TabKind,
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub shared: Arc<SessionShared>,
    pub shell_pid: i32,
    /// Manual placement in the tab tree (a group key); `None` = classified automatically.
    pub group_override: Option<String>,
    pub bell_at: Option<Instant>,
    pub program_label: String,
    cols: u16,
    rows: u16,
    cell_px: (u16, u16),
    writer: Sender<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: Arc<Mutex<WindowSize>>,
    registry: Arc<ScanRegistry>,
    exited: Option<i32>,
}

pub fn default_shell() -> String {
    CommandBuilder::new_default_prog().get_shell()
}

impl Session {
    pub fn spawn(
        id: TabId,
        req: SpawnRequest,
        ctx: egui::Context,
        registry: Arc<ScanRegistry>,
        palette: Arc<Palette>,
    ) -> Result<Session> {
        let cols = req.cols.max(2);
        let rows = req.rows.max(1);
        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: cols.saturating_mul(req.cell_px.0),
            pixel_height: rows.saturating_mul(req.cell_px.1),
        };
        let pair = native_pty_system()
            .openpty(pty_size)
            .context("openpty failed")?;

        let program = req.program.clone().unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&program);
        for a in &req.args {
            cmd.arg(a);
        }
        let shell_name = Path::new(&program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&program)
            .to_string();
        let is_interactive_shell = req.args.is_empty()
            && (req.program.is_none() || matches!(shell_name.as_str(), "bash" | "zsh" | "fish"));
        if is_interactive_shell && let Some(integration) = &req.integration {
            integration.configure(&program, &mut cmd);
        }
        cmd.env("TERM", &req.term_env);
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", crate::APP_NAME);
        cmd.env("TERM_PROGRAM_VERSION", crate::APP_VERSION);
        cmd.env("VERTERM_TAB_ID", id.to_string());

        let cwd = req
            .cwd
            .filter(|p| p.is_dir())
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()));
        if let Some(cwd) = &cwd {
            cmd.cwd(cwd);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning {program}"))?;
        drop(pair.slave);
        let shell_pid = child.process_id().context("child has no pid")? as i32;
        let master = pair.master;

        let mut reader = master.try_clone_reader().context("clone pty reader")?;
        let mut writer = master.take_writer().context("take pty writer")?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();

        let shared = Arc::new(SessionShared {
            id,
            shell_pid,
            state: Mutex::new(SessionState::default()),
        });
        let size = Arc::new(Mutex::new(WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: req.cell_px.0,
            cell_height: req.cell_px.1,
        }));
        let proxy = EventProxy {
            ctx: ctx.clone(),
            writer: tx.clone(),
            shared: shared.clone(),
            palette,
            size: size.clone(),
        };
        let term_config = TermConfig {
            scrolling_history: req.scrollback,
            ..TermConfig::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &GridSize {
                cols: cols as usize,
                lines: rows as usize,
            },
            proxy,
        )));

        // Writer thread: serialises all PTY input; a blocked write never stalls the GUI.
        thread::Builder::new()
            .name(format!("pty-writer-{id}"))
            .spawn(move || {
                for bytes in rx {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            })
            .context("spawn writer thread")?;

        // Reader thread: PTY → OSC tap → vte → Term, then wake the GUI.
        {
            let term = term.clone();
            let shared = shared.clone();
            let ctx = ctx.clone();
            thread::Builder::new()
                .name(format!("pty-reader-{id}"))
                .spawn(move || reader_loop(&mut *reader, term, shared, ctx))
                .context("spawn reader thread")?;
        }

        // Scanner registration with an independent dup of the master fd (tcgetpgrp).
        let raw_fd = master.as_raw_fd().context("master fd")?;
        let scan_fd =
            nix::unistd::dup(unsafe { BorrowedFd::borrow_raw(raw_fd) }).context("dup master fd")?;
        registry.add(ScanTarget {
            id,
            shell_pid,
            master_fd: scan_fd,
            shared: shared.clone(),
        });

        let program_label = if req.args.is_empty() {
            shell_name
        } else {
            format!("{shell_name} {}", req.args.join(" "))
        };

        Ok(Session {
            id,
            kind: req.kind,
            term,
            shared,
            shell_pid,
            group_override: None,
            bell_at: None,
            program_label,
            cols,
            rows,
            cell_px: req.cell_px,
            writer: tx,
            master,
            child,
            size,
            registry,
            exited: None,
        })
    }

    pub fn write(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        let _ = self.writer.send(bytes);
    }

    /// Paste text the way a terminal should: bracketed when the application asked for it,
    /// otherwise with newlines normalised to carriage returns.
    pub fn paste(&self, text: &str) {
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let mut out = Vec::with_capacity(text.len() + 16);
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(text.replace("\x1b[201~", "").as_bytes());
            out.extend_from_slice(b"\x1b[201~");
        } else {
            out.extend_from_slice(text.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
        }
        self.write(out);
    }

    pub fn grid_size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_px: (u16, u16)) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows && cell_px == self.cell_px {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.cell_px = cell_px;
        self.term.lock().resize(GridSize {
            cols: cols as usize,
            lines: rows as usize,
        });
        *self.size.lock() = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: cell_px.0,
            cell_height: cell_px.1,
        };
        if let Err(e) = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: cols.saturating_mul(cell_px.0),
            pixel_height: rows.saturating_mul(cell_px.1),
        }) {
            tracing::warn!(tab = self.id, "pty resize failed: {e}");
        }
    }

    /// Non-blocking; returns the exit code once the child is gone.
    pub fn poll_exit(&mut self) -> Option<i32> {
        if let Some(code) = self.exited {
            return Some(code);
        }
        let reader_saw_eof = self.shared.state.lock().shell_exited.is_some();
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = status.exit_code() as i32;
                self.exited = Some(code);
                self.shared.state.lock().shell_exited = Some(Some(code));
                Some(code)
            }
            Ok(None) if reader_saw_eof => {
                // PTY closed but the process lingers (e.g. reparented); treat as exited.
                None
            }
            _ => None,
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.registry.remove(self.id);
        let _ = self.child.kill();
    }
}

// ---------------------------------------------------------------------------------------
// Reader thread
// ---------------------------------------------------------------------------------------

fn reader_loop(
    reader: &mut dyn Read,
    term: Arc<FairMutex<Term<EventProxy>>>,
    shared: Arc<SessionShared>,
    ctx: egui::Context,
) {
    let mut processor: Processor = Processor::new();
    let mut tap = OscTap::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut filtered = Vec::with_capacity(64 * 1024);
    let mut events: Vec<OscMark> = Vec::new();
    let mut positioned: Vec<(OscEvent, GridPos)> = Vec::new();

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        filtered.clear();
        events.clear();
        tap.process(&buf[..n], &mut filtered, &mut events);

        positioned.clear();
        {
            let mut t = term.lock();
            let mut pos = 0usize;
            for mark in &events {
                if mark.at > pos {
                    processor.advance(&mut *t, &filtered[pos..mark.at]);
                    pos = mark.at;
                }
                positioned.push((mark.event.clone(), GridPos::sample(&t)));
            }
            processor.advance(&mut *t, &filtered[pos..]);
        }
        if !positioned.is_empty() {
            apply_osc_events(&shared, &positioned);
        }
        ctx.request_repaint();
    }

    let mut st = shared.state.lock();
    if st.shell_exited.is_none() {
        st.shell_exited = Some(None);
    }
    drop(st);
    tracing::debug!(tab = shared.id, pid = shared.shell_pid, "pty closed");
    ctx.request_repaint();
}

fn local_hostname() -> &'static str {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    })
}

fn is_local_host(host: &str) -> bool {
    if host.is_empty() || host == "localhost" {
        return true;
    }
    let local = local_hostname();
    host.eq_ignore_ascii_case(local)
        || host
            .split('.')
            .next()
            .is_some_and(|h| h.eq_ignore_ascii_case(local.split('.').next().unwrap_or("")))
}

/// Where an OSC mark landed in the grid, sampled with the parser advanced to exactly that
/// byte and no further.
#[derive(Clone, Copy, Debug, Default)]
pub struct GridPos {
    /// Viewport row — what the overlay anchors to, valid only until the grid scrolls.
    pub row: usize,
    /// `history_size + screen line`. Scrolling pushes lines into the history and grows
    /// `history_size` by the same amount, so this number stays attached to its text; the
    /// alacritty `Line` for it later is `abs - history_size()` (negative = in the
    /// scrollback). It only drifts once the scrollback is full and lines start being
    /// evicted, which readers handle by clamping to `topmost_line`.
    pub abs: i64,
}

impl GridPos {
    fn sample(term: &Term<EventProxy>) -> Self {
        let grid = term.grid();
        let line = grid.cursor.point.line.0;
        Self {
            row: (line + grid.display_offset() as i32).max(0) as usize,
            abs: grid.history_size() as i64 + line as i64,
        }
    }
}

fn apply_osc_events(shared: &SessionShared, events: &[(OscEvent, GridPos)]) {
    let mut st = shared.state.lock();
    for (ev, at) in events {
        match ev {
            OscEvent::Cwd { host, path } => {
                if is_local_host(host) {
                    st.osc_cwd = Some(PathBuf::from(path));
                    st.remote_cwd = None;
                } else {
                    st.remote_cwd = Some((host.clone(), path.clone()));
                }
            }
            OscEvent::PromptStart => {
                st.osc133_seen = true;
                st.prompt_line = Some(at.row);
                st.prompt_abs = Some(at.abs);
            }
            OscEvent::PromptEnd => {}
            OscEvent::CommandStart(cmd) => {
                st.osc133_seen = true;
                st.command_abs = Some(at.abs);
                st.start_command(cmd.clone(), RunSource::Osc133);
            }
            OscEvent::CommandEnd(code) => {
                st.osc133_seen = true;
                st.finish_command(*code);
            }
        }
    }
}

//! verterm — a GPU-accelerated, keyboard-centric Linux terminal emulator with a
//! vertical tab tree, `/proc`-driven session classification and summon-only local AI.
//!
//! Module map (CLAUDE.md carries the routing table for humans and agents):
//! - `session`           PTY spawn, reader/writer threads, alacritty `Term` ownership
//! - `osc`               pre-parser tap extracting OSC 7 / OSC 133 before vte sees the stream
//! - `procscan`          /proc scanner: SSH / root classification, foreground job, CPU & RSS
//! - `workspace`         tab tree (bucket → group → tab), project-root and git detection
//! - `ai`                Ollama client (streaming), context builder, command extraction
//! - `input`             egui key events → terminal byte sequences
//! - `keymap`            chords → actions, defaults + config overrides
//! - `hints`             fast-jump regex hinting (URL / path / IP / UUID / git hash)
//! - `notify`            desktop notifications over the session D-Bus
//! - `shell_integration` bash / zsh / fish hook scripts and their injection
//! - `tabsearch`         finding a session by identity, or by what its scrollback contains
//! - `ui`                eframe app: rail, terminal view, overlays, theme, fonts

mod ai;
mod config;
mod hints;
mod input;
mod keymap;
mod notify;
mod osc;
mod procscan;
mod session;
mod shell_integration;
mod tabsearch;
mod ui;
mod workspace;

use std::path::PathBuf;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

pub const APP_NAME: &str = "verterm";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Command-line options. Kept dependency-free on purpose; the surface is tiny.
#[derive(Debug, Clone, Default)]
pub struct Cli {
    pub config: Option<PathBuf>,
    pub working_directory: Option<PathBuf>,
    /// Program + args to run instead of the login shell (`-e cmd args...`).
    pub command: Vec<String>,
    pub print_default_config: bool,
}

const USAGE: &str = "\
verterm — GPU-accelerated, keyboard-centric Linux terminal

USAGE:
    verterm [OPTIONS] [-e <command> [args...]]

OPTIONS:
    -c, --config <file>              Use this config file instead of $XDG_CONFIG_HOME/verterm/config.toml
    -d, --working-directory <dir>    Start the first tab in this directory
    -e, --command <cmd> [args...]    Run <cmd> instead of the shell (everything after -e is the command)
        --print-default-config       Print the annotated default config.toml and exit
    -h, --help                       Show this help
    -V, --version                    Show version
";

fn parse_cli() -> Cli {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => cli.config = args.next().map(PathBuf::from),
            "-d" | "--working-directory" => cli.working_directory = args.next().map(PathBuf::from),
            "-e" | "--command" => {
                cli.command = args.by_ref().collect();
            }
            "--print-default-config" => cli.print_default_config = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("{APP_NAME} {APP_VERSION}");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    cli
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("verterm=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn main() -> Result<()> {
    let cli = parse_cli();
    if cli.print_default_config {
        print!("{}", config::DEFAULT_CONFIG_TOML);
        return Ok(());
    }
    init_tracing();

    let config = config::Config::load(cli.config.as_deref())?;
    tracing::info!(
        version = APP_VERSION,
        config = %config::Config::default_path().map(|p| p.display().to_string()).unwrap_or_default(),
        "starting"
    );

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id(APP_NAME)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([320.0, 200.0]),
        renderer: eframe::Renderer::Wgpu,
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        native_options,
        Box::new(move |cc| {
            ui::App::new(cc, config, cli)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(|e| e.into())
        }),
    )
    .map_err(|e| anyhow::anyhow!("failed to start the UI: {e}"))
}

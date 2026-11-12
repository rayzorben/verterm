//! Pinned sessions: saved recipes for opening a tab — a local folder, an SSH host, a
//! container, or any command — kept in `$XDG_CONFIG_HOME/verterm/sessions.toml`.
//!
//! The file is separate from `config.toml` on purpose: verterm never rewrites the file the
//! user maintains by hand, and pins are added, edited and removed from inside the app. It is
//! still plain TOML and is reloaded live when edited by hand (comments do not survive a save
//! from the app, which the header says).
//!
//! A pin never becomes the tab's *program*. The tab runs the user's login shell as usual and
//! the pin's command is typed into it once the first prompt is up (see `App::poll_sessions`).
//! That is load-bearing: the `/proc` scanner classifies a tab by the processes *under* its
//! shell, so `ssh` exec'd as the tab's root process would be classified as a local session; a
//! shell with arguments also gets no shell integration; and a dropped connection would close
//! the tab instead of leaving a prompt to reconnect from.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::keymap::Chord;

/// What a pin opens. Each kind is sugar for one shell command; `Command` is the general case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PinKind {
    /// A shell in `dir`, nothing typed.
    #[default]
    Local,
    /// `ssh <target>`; `target` is shell text, so `-p 2222 me@host` works.
    Ssh,
    /// `distrobox enter <container>`.
    Container,
    /// Any command line.
    Command,
}

impl PinKind {
    pub const ALL: [PinKind; 4] = [
        PinKind::Local,
        PinKind::Ssh,
        PinKind::Container,
        PinKind::Command,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PinKind::Local => "local",
            PinKind::Ssh => "ssh",
            PinKind::Container => "container",
            PinKind::Command => "command",
        }
    }

    /// What the kind's one text field is called in the edit form.
    pub fn target_label(self) -> Option<&'static str> {
        match self {
            PinKind::Local => None,
            PinKind::Ssh => Some("host"),
            PinKind::Container => Some("container"),
            PinKind::Command => Some("command"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pin {
    /// Unique, case-insensitively. Also how open tabs remember which pin they came from.
    pub name: String,
    pub kind: PinKind,
    /// Local start directory (`~` is expanded). For ssh and container pins this is the
    /// directory on *this* machine the shell starts in before the command runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// `ssh` destination and options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// distrobox container name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Command line for `kind = "command"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Open this pin when verterm starts.
    #[serde(skip_serializing_if = "is_false")]
    pub autostart: bool,
    /// Close the tab when the command ends (appends `; exit`) instead of dropping back to the
    /// local shell, which is where a disconnected ssh session leaves you by default.
    #[serde(skip_serializing_if = "is_false")]
    pub exit_with_command: bool,
    /// Chord that opens (or focuses) this pin, e.g. `"Alt+Shift+1"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

impl Pin {
    /// The kind's own text field (host, container name or command), if it has one.
    pub fn target_text(&self) -> Option<&str> {
        match self.kind {
            PinKind::Local => None,
            PinKind::Ssh => nonempty(&self.target),
            PinKind::Container => nonempty(&self.container),
            PinKind::Command => nonempty(&self.command),
        }
    }

    /// The line typed into the shell once its first prompt is up; `None` for a plain folder.
    pub fn launch_command(&self) -> Option<String> {
        let base = match self.kind {
            PinKind::Local => return None,
            PinKind::Ssh => format!("ssh {}", self.target_text()?),
            PinKind::Container => format!("distrobox enter {}", shell_quote(self.target_text()?)),
            PinKind::Command => self.target_text()?.to_string(),
        };
        Some(if self.exit_with_command {
            format!("{base}; exit")
        } else {
            base
        })
    }

    /// `dir` with a leading `~` expanded against `home`.
    pub fn start_dir(&self, home: &Path) -> Option<PathBuf> {
        let d = nonempty(&self.dir)?;
        Some(expand_home(d, home))
    }

    /// One line describing what the pin does, for the launcher and the rail tooltip.
    pub fn summary(&self) -> String {
        match self.launch_command() {
            Some(cmd) => cmd,
            None => nonempty(&self.dir).unwrap_or("~").to_string(),
        }
    }

    pub fn chord(&self) -> Option<Result<Chord>> {
        nonempty(&self.key).map(Chord::parse)
    }

    /// Drop the fields another kind would use and blank strings, so the saved file says only
    /// what this pin does.
    pub fn normalized(mut self) -> Self {
        let trim = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        self.name = self.name.trim().to_string();
        self.dir = trim(self.dir);
        self.key = trim(self.key);
        self.target = trim(self.target).filter(|_| self.kind == PinKind::Ssh);
        self.container = trim(self.container).filter(|_| self.kind == PinKind::Container);
        self.command = trim(self.command).filter(|_| self.kind == PinKind::Command);
        self
    }

    /// Why this pin cannot be saved, if it cannot. `others` is every *other* pin.
    pub fn validate<'a>(&self, mut others: impl Iterator<Item = &'a Pin>) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("give the pin a name");
        }
        if others.any(|p| p.name.eq_ignore_ascii_case(self.name.trim())) {
            bail!("a pin named {:?} already exists", self.name.trim());
        }
        if let Some(field) = self.kind.target_label()
            && self.target_text().is_none()
        {
            bail!("a {} pin needs a {field}", self.kind.label());
        }
        if let Some(Err(e)) = self.chord() {
            bail!("key: {e}");
        }
        Ok(())
    }
}

pub fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Same rule as `ui::shell_quote`; duplicated so this module has no UI dependency.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@~+=,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PinFile {
    #[serde(rename = "session")]
    pub pins: Vec<Pin>,
}

const HEADER: &str = "\
# verterm pinned sessions. Written by verterm when you add, edit or unpin a session from the
# app (Ctrl+Shift+O, or right-click in the rail); edits made here by hand are picked up live,
# but comments are not kept when verterm saves.
#
# kind = local | ssh | container | command
#   local      dir = \"~/code/project\"
#   ssh        target = \"me@host\"          (runs: ssh me@host)
#   container  container = \"name\"          (runs: distrobox enter name)
#   command    command = \"journalctl -f\"
# Optional: dir, autostart = true, exit_with_command = true, key = \"Alt+Shift+1\"

";

impl PinFile {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", crate::APP_NAME)
            .map(|d| d.config_dir().join("sessions.toml"))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: PinFile = toml::from_str(text)?;
        Ok(file)
    }

    /// A missing file is an empty list; an unreadable one is an error so a typo is reported
    /// rather than silently showing no pins.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn render(&self) -> Result<String> {
        Ok(format!(
            "{HEADER}{}",
            toml::to_string_pretty(self).context("encoding pins")?
        ))
    }

    pub fn find(&self, name: &str) -> Option<&Pin> {
        self.pins.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }
}

/// Writes the pin file on one background thread. One thread, not one per save: two detached
/// writers could land out of order and leave the older list on disk. Each write goes to a
/// temporary file renamed over the real one, so the config watcher never reads half a file.
pub struct PinWriter {
    tx: Sender<PinFile>,
}

impl PinWriter {
    pub fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel::<PinFile>();
        let spawned = std::thread::Builder::new()
            .name("pin-writer".into())
            .spawn(move || {
                while let Ok(mut file) = rx.recv() {
                    // Only the newest list matters.
                    while let Ok(newer) = rx.try_recv() {
                        file = newer;
                    }
                    if let Err(e) = write_atomic(&path, &file) {
                        tracing::warn!("could not save pinned sessions: {e:#}");
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("pin writer thread failed to start: {e}");
        }
        Self { tx }
    }

    pub fn save(&self, file: &PinFile) {
        let _ = self.tx.send(file.clone());
    }
}

fn write_atomic(path: &Path, file: &PinFile) -> Result<()> {
    let dir = path.parent().context("pin file has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(".sessions.toml.tmp");
    std::fs::write(&tmp, file.render()?).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(kind: PinKind) -> Pin {
        Pin {
            name: "x".into(),
            kind,
            ..Pin::default()
        }
    }

    #[test]
    fn each_kind_builds_its_command() {
        let mut p = pin(PinKind::Ssh);
        p.target = Some("rayben@orohost".into());
        assert_eq!(p.launch_command().as_deref(), Some("ssh rayben@orohost"));

        let mut p = pin(PinKind::Container);
        p.container = Some("sedanos".into());
        assert_eq!(
            p.launch_command().as_deref(),
            Some("distrobox enter sedanos")
        );
        p.container = Some("my box".into());
        assert_eq!(
            p.launch_command().as_deref(),
            Some("distrobox enter 'my box'")
        );

        let mut p = pin(PinKind::Command);
        p.command = Some("journalctl -f".into());
        p.exit_with_command = true;
        assert_eq!(p.launch_command().as_deref(), Some("journalctl -f; exit"));

        let mut p = pin(PinKind::Local);
        p.dir = Some("~/code/mudengine".into());
        p.command = Some("ignored".into());
        assert_eq!(p.launch_command(), None, "a folder pin types nothing");
    }

    #[test]
    fn ssh_target_is_shell_text_not_quoted() {
        let mut p = pin(PinKind::Ssh);
        p.target = Some("-p 2222 me@host".into());
        assert_eq!(p.launch_command().as_deref(), Some("ssh -p 2222 me@host"));
    }

    #[test]
    fn start_dir_expands_home() {
        let home = Path::new("/home/u");
        let mut p = pin(PinKind::Local);
        p.dir = Some("~/code/mudengine".into());
        assert_eq!(
            p.start_dir(home),
            Some(PathBuf::from("/home/u/code/mudengine"))
        );
        p.dir = Some("~".into());
        assert_eq!(p.start_dir(home), Some(PathBuf::from("/home/u")));
        p.dir = Some("/srv".into());
        assert_eq!(p.start_dir(home), Some(PathBuf::from("/srv")));
        p.dir = Some("  ".into());
        assert_eq!(p.start_dir(home), None);
    }

    #[test]
    fn the_documented_file_parses() {
        let text = r#"
[[session]]
name = "mudengine"
kind = "local"
dir = "~/code/mudengine"
autostart = true
key = "Alt+Shift+1"

[[session]]
name = "orohost"
kind = "ssh"
target = "rayben@orohost"

[[session]]
name = "sedanos"
kind = "container"
container = "sedanos"
"#;
        let f = PinFile::parse(text).unwrap();
        assert_eq!(f.pins.len(), 3);
        assert!(f.pins[0].autostart);
        assert_eq!(f.pins[0].chord().unwrap().unwrap().display(), "Alt+Shift+1");
        assert_eq!(f.find("OROHOST").unwrap().kind, PinKind::Ssh);
        assert!(PinFile::parse("[[session]]\nname = \"a\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn render_round_trips_and_keeps_the_header() {
        let mut p = pin(PinKind::Ssh);
        p.target = Some("h".into());
        p.autostart = true;
        let f = PinFile { pins: vec![p] };
        let text = f.render().unwrap();
        assert!(text.starts_with("# verterm pinned sessions"));
        let body = text.strip_prefix(HEADER).expect("header first");
        assert!(
            !body.contains("exit_with_command"),
            "false flags are omitted"
        );
        assert_eq!(PinFile::parse(&text).unwrap(), f);
    }

    #[test]
    fn normalizing_drops_other_kinds_fields() {
        let p = Pin {
            name: "  oro ".into(),
            kind: PinKind::Ssh,
            target: Some(" h ".into()),
            container: Some("c".into()),
            command: Some("x".into()),
            dir: Some("".into()),
            ..Pin::default()
        }
        .normalized();
        assert_eq!(p.name, "oro");
        assert_eq!(p.target.as_deref(), Some("h"));
        assert_eq!((p.container, p.command, p.dir), (None, None, None));
    }

    #[test]
    fn validation() {
        let existing = [Pin {
            name: "Oro".into(),
            ..Pin::default()
        }];
        let mut p = pin(PinKind::Ssh);
        assert!(p.validate(existing.iter()).is_err(), "ssh needs a host");
        p.target = Some("h".into());
        assert!(p.validate(existing.iter()).is_ok());
        p.name = "oro".into();
        assert!(p.validate(existing.iter()).is_err(), "names are unique");
        p.name = "new".into();
        p.key = Some("Alt+Nope".into());
        assert!(p.validate(existing.iter()).is_err(), "bad chord");
        p.name = " ".into();
        assert!(p.validate(std::iter::empty()).is_err());
    }
}

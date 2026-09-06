//! Ship and inject the shell hooks that emit OSC 7 (cwd) and OSC 133 (prompt / command
//! boundaries with exit codes). Without them verterm still works: the `/proc` scanner infers
//! command start/stop, but exit codes are unavailable.
//!
//! Injection follows the kitty/wezterm approach so the user's own rc files run unmodified:
//! - bash: `bash --rcfile <dir>/verterm.bash` (which sources the normal rc files first)
//! - zsh:  `ZDOTDIR=<dir>/zsh` whose `.zshenv` restores the real ZDOTDIR and adds hooks
//! - fish: `fish --init-command 'source <dir>/verterm.fish'`

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use portable_pty::CommandBuilder;

const BASH: &str = include_str!("../shell-integration/verterm.bash");
const ZSH_ENV: &str = include_str!("../shell-integration/zsh/.zshenv");
const ZSH: &str = include_str!("../shell-integration/verterm.zsh");
const FISH: &str = include_str!("../shell-integration/verterm.fish");

#[derive(Debug, Clone)]
pub struct ShellIntegration {
    dir: PathBuf,
}

impl ShellIntegration {
    /// Write the scripts under `$XDG_DATA_HOME/verterm/shell-integration` (idempotent).
    pub fn install() -> Result<Self> {
        let dirs =
            directories::ProjectDirs::from("", "", crate::APP_NAME).context("no XDG dirs")?;
        let dir = dirs.data_dir().join("shell-integration");
        std::fs::create_dir_all(dir.join("zsh"))
            .with_context(|| format!("creating {}", dir.display()))?;
        write_if_changed(&dir.join("verterm.bash"), BASH)?;
        write_if_changed(&dir.join("verterm.zsh"), ZSH)?;
        write_if_changed(&dir.join("zsh").join(".zshenv"), ZSH_ENV)?;
        write_if_changed(&dir.join("verterm.fish"), FISH)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Adjust `cmd` (argv + env) for `shell_path`. Returns whether hooks were injected.
    pub fn configure(&self, shell_path: &str, cmd: &mut CommandBuilder) -> bool {
        cmd.env("VERTERM_SHELL_INTEGRATION_DIR", &self.dir);
        let name = Path::new(shell_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        match name {
            "bash" => {
                cmd.arg("--rcfile");
                cmd.arg(self.dir.join("verterm.bash"));
                true
            }
            "zsh" => {
                let orig = std::env::var("ZDOTDIR").unwrap_or_default();
                cmd.env("VERTERM_ORIG_ZDOTDIR", orig);
                cmd.env("ZDOTDIR", self.dir.join("zsh"));
                true
            }
            "fish" => {
                cmd.arg("--init-command");
                cmd.arg(format!(
                    "source '{}'",
                    self.dir.join("verterm.fish").display()
                ));
                true
            }
            _ => false,
        }
    }
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if std::fs::read_to_string(path)
        .map(|c| c == content)
        .unwrap_or(false)
    {
        return Ok(());
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

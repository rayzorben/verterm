//! Desktop notifications over the session D-Bus (`org.freedesktop.Notifications`), which is
//! what mako / dunst / swaync implement on Wayland. Callers decide *whether* to notify (the
//! tab must be unfocused and the command must have run long enough); this module only
//! formats and dispatches, always from a helper thread so a slow daemon never blocks the GUI.

use std::sync::mpsc::Sender;
use std::time::Duration;

use notify_rust::{Notification, Timeout, Urgency};

use crate::session::TabId;

pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", d.as_secs_f32())
    }
}

/// Human hint for the well-known exit codes a sysadmin cares about.
pub fn describe_exit(code: i32) -> &'static str {
    match code {
        124 => " (timeout)",
        126 => " (not executable)",
        127 => " (command not found)",
        130 => " (interrupted)",
        137 => " (SIGKILL / OOM-killed)",
        139 => " (segfault)",
        143 => " (SIGTERM)",
        _ => "",
    }
}

/// How an activated notification gets the user back to the session it came from.
///
/// The tab id travels to the GUI thread over a channel rather than being applied here: this is
/// the notify thread, and it must not touch app state.
pub struct Activation {
    pub tab: TabId,
    pub tx: Sender<TabId>,
    /// The window is unfocused whenever a notification fires (that is the policy), so it may
    /// not be drawing. Without this the activation would sit in the channel unseen.
    pub ctx: egui::Context,
}

pub fn notify_command_complete(
    tab_title: &str,
    command: &str,
    exit_code: Option<i32>,
    elapsed: Duration,
    timeout_ms: u32,
    activate: Option<Activation>,
) {
    let duration = format_duration(elapsed);
    let cmd = if command.is_empty() {
        "command".to_string()
    } else {
        format!("`{command}`")
    };
    let (summary, body, urgency, icon) = match exit_code {
        Some(0) | None => (
            format!("Finished: {tab_title}"),
            format!("{cmd} finished in {duration}"),
            Urgency::Normal,
            "utilities-terminal",
        ),
        Some(code) => (
            format!("Failed (exit {code}): {tab_title}"),
            format!("{cmd} failed after {duration}{}", describe_exit(code)),
            Urgency::Critical,
            "dialog-error",
        ),
    };
    dispatch(summary, body, urgency, icon, timeout_ms, activate);
}

/// Everything the UI knows about the tab that rang, so the body can say *what* rang rather
/// than that *something* did. Assembled in `poll_sessions` while the `SessionState` lock is
/// held; formatting and dispatch happen after it is dropped.
#[derive(Clone, Debug, Default)]
pub struct BellContext {
    /// Tab title (OSC 0/2, else foreground comm, else cwd) — the notification summary.
    pub tab_title: String,
    /// 1-based position in the rail, so a wall of terminals is still navigable.
    pub tab_index: usize,
    /// Foreground job on the PTY (`vim`, `make`, …) — absent means the shell itself rang.
    pub foreground: Option<String>,
    /// Command the lifecycle tracker believes is running, and for how long.
    pub running: Option<(String, Duration)>,
    /// Shell program label, used when nothing more specific is known (`bash`, `fish`).
    pub program: String,
    /// Shell pid, the last-resort way to identify which terminal this is.
    pub shell_pid: i32,
    /// Directory, already shortened to `~/…` for display.
    pub cwd: Option<String>,
    /// `Some(host)` when the tab is an SSH/mosh session.
    pub remote_host: Option<String>,
    /// True when the session is root / sudo-elevated.
    pub elevated: bool,
    /// Command that finished just before the bell, if any (shells ring on completion).
    pub last_exit_code: Option<i32>,
}

/// `bash · make` / `ssh host · vim` — who rang, most specific name first.
fn bell_actor(c: &BellContext) -> String {
    let mut who = String::new();
    if let Some(host) = &c.remote_host {
        who.push_str(host);
        who.push_str(": ");
    }
    if c.elevated {
        who.push_str("root ");
    }
    match (&c.foreground, &c.running) {
        // A live foreground job is the most trustworthy answer.
        (Some(fg), _) => who.push_str(fg),
        // No foreground job, but the lifecycle tracker still has a command: name it.
        (None, Some((cmd, _))) if !cmd.is_empty() => who.push_str(cmd),
        // Nothing running: the shell rang at its own prompt.
        _ => who.push_str(if c.program.is_empty() {
            "shell"
        } else {
            &c.program
        }),
    }
    who
}

/// Multi-line body: who rang · for how long, where it happened, and how to find the tab.
pub fn format_bell_body(c: &BellContext) -> String {
    let mut first = bell_actor(c);
    match &c.running {
        Some((_, elapsed)) => {
            first.push_str(&format!(" · running {}", format_duration(*elapsed)));
        }
        // Not running, but something finished: the bell is almost certainly that completion.
        None => match c.last_exit_code {
            Some(0) => first.push_str(" · at the prompt"),
            Some(code) => {
                first.push_str(&format!(" · last exit {code}{}", describe_exit(code)));
            }
            None => first.push_str(" · at the prompt"),
        },
    }

    let mut lines = vec![first];
    if let Some(cwd) = &c.cwd {
        lines.push(format!("in {cwd}"));
    }
    let mut trailer = String::new();
    if c.tab_index > 0 {
        trailer.push_str(&format!("tab {}", c.tab_index));
    }
    if c.shell_pid != 0 {
        if !trailer.is_empty() {
            trailer.push_str(" · ");
        }
        trailer.push_str(&format!("pid {}", c.shell_pid));
    }
    if !trailer.is_empty() {
        lines.push(trailer);
    }
    lines.join("\n")
}

pub fn notify_bell(c: &BellContext, timeout_ms: u32, activate: Option<Activation>) {
    let summary = if c.tab_title.is_empty() {
        "Bell".to_string()
    } else {
        format!("Bell: {}", c.tab_title)
    };
    dispatch(
        summary,
        format_bell_body(c),
        Urgency::Normal,
        "utilities-terminal",
        timeout_ms,
        activate,
    );
}

fn dispatch(
    summary: String,
    body: String,
    urgency: Urgency,
    icon: &'static str,
    timeout_ms: u32,
    activate: Option<Activation>,
) {
    std::thread::Builder::new()
        .name("notify".into())
        .spawn(move || {
            let mut n = Notification::new();
            n.appname(crate::APP_NAME)
                .summary(&summary)
                .body(&body)
                .icon(icon)
                .urgency(urgency)
                .timeout(Timeout::Milliseconds(timeout_ms));
            if activate.is_some() {
                // Exactly one action, keyed `default`. That is the XDG key for "the body was
                // clicked", so mako/dunst/GNOME activate it without drawing a button; daemons
                // that ignore the convention and draw every action (quickshell's
                // DankMaterialShell renders the whole list and invokes `actions[0]` on a body
                // click) then draw one button labelled "Show session". A second, differently
                // keyed action would only be a duplicate button on the latter.
                n.action("default", "Show session");
            }
            let handle = match n.show() {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("desktop notification failed: {e}");
                    return;
                }
            };
            let Some(a) = activate else { return };
            // Blocks until the notification is actioned, dismissed or expires, then this
            // thread ends — it also matches `NotificationClosed`, so nothing is leaked when
            // the notification simply times out.
            handle.wait_for_action(|action| {
                if action == "default" {
                    let _ = a.tx.send(a.tab);
                    a.ctx.request_repaint();
                }
            });
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> BellContext {
        BellContext {
            tab_title: "fish".into(),
            tab_index: 3,
            program: "fish".into(),
            shell_pid: 4242,
            cwd: Some("~/src/verterm".into()),
            ..Default::default()
        }
    }

    #[test]
    fn bell_names_the_foreground_job_and_its_runtime() {
        let c = BellContext {
            foreground: Some("make".into()),
            running: Some(("make -j8".into(), Duration::from_secs(252))),
            ..ctx()
        };
        assert_eq!(
            format_bell_body(&c),
            "make · running 4m 12s\nin ~/src/verterm\ntab 3 · pid 4242"
        );
    }

    #[test]
    fn bell_at_the_prompt_falls_back_to_the_shell() {
        assert_eq!(
            format_bell_body(&ctx()),
            "fish · at the prompt\nin ~/src/verterm\ntab 3 · pid 4242"
        );
    }

    #[test]
    fn bell_reports_the_failure_that_probably_caused_it() {
        let c = BellContext {
            last_exit_code: Some(127),
            ..ctx()
        };
        assert!(
            format_bell_body(&c).starts_with("fish · last exit 127 (command not found)"),
            "{}",
            format_bell_body(&c)
        );
    }

    #[test]
    fn bell_labels_remote_and_elevated_sessions() {
        let c = BellContext {
            remote_host: Some("build01".into()),
            elevated: true,
            foreground: Some("apt".into()),
            ..ctx()
        };
        assert!(format_bell_body(&c).starts_with("build01: root apt"));
    }

    #[test]
    fn bell_names_a_running_command_with_no_foreground_job() {
        let c = BellContext {
            running: Some(("cargo test".into(), Duration::from_secs(5))),
            ..ctx()
        };
        assert!(format_bell_body(&c).starts_with("cargo test · running 5.0s"));
    }

    #[test]
    fn bell_body_survives_a_session_we_know_nothing_about() {
        assert_eq!(
            format_bell_body(&BellContext::default()),
            "shell · at the prompt"
        );
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_millis(3200)), "3.2s");
        assert_eq!(format_duration(Duration::from_secs(252)), "4m 12s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h 02m");
    }
}

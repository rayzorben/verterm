//! Background `/proc` scanner (500 ms cadence by default). For every live session it:
//! 1. walks the shell's descendant tree (`ppid` links from `/proc/*/stat`),
//! 2. classifies the session — SSH / mosh / telnet ⇒ `RemoteSSH`, sudo / doas / su or any
//!    descendant with uid 0 ⇒ `ElevatedRoot`, else `Local`,
//! 3. finds the foreground job via `tcgetpgrp` on the PTY master,
//! 4. aggregates CPU% (jiffie deltas) and RSS over the foreground tree,
//! 5. reads the shell cwd, detects the project root, and refreshes cached git info,
//! 6. drives the command lifecycle when shell integration (OSC 133) is absent.
//!
//! It never touches the terminal grid and never blocks the GUI thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::session::{ForegroundProc, GitInfo, ProcessCategory, RunSource, SessionShared, TabId};
use crate::workspace::detect_project_root;

pub struct ScanTarget {
    pub id: TabId,
    pub shell_pid: i32,
    pub master_fd: OwnedFd,
    pub shared: Arc<SessionShared>,
}

#[derive(Default)]
pub struct ScanRegistry {
    targets: Mutex<Vec<Arc<ScanTarget>>>,
}

impl ScanRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add(&self, target: ScanTarget) {
        self.targets.lock().push(Arc::new(target));
    }

    pub fn remove(&self, id: TabId) {
        self.targets.lock().retain(|t| t.id != id);
    }

    fn snapshot(&self) -> Vec<Arc<ScanTarget>> {
        self.targets.lock().clone()
    }
}

pub fn spawn_scanner(
    registry: Arc<ScanRegistry>,
    ctx: egui::Context,
    interval: Duration,
    home: PathBuf,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("proc-scanner".into())
        .spawn(move || {
            Scanner {
                registry,
                ctx,
                interval,
                home,
                cpu_prev: HashMap::new(),
                git_cache: HashMap::new(),
                page_size: procfs::page_size(),
                ticks_per_second: procfs::ticks_per_second().max(1),
                cpu_count: std::thread::available_parallelism()
                    .map(|n| n.get() as f32)
                    .unwrap_or(1.0),
            }
            .run()
        })
        .expect("spawn proc-scanner thread")
}

// ---------------------------------------------------------------------------------------
// Process table snapshot
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct ProcInfo {
    pid: i32,
    pgrp: i32,
    comm: String,
    jiffies: u64,
    rss_pages: u64,
    uid: Option<u32>,
}

struct ProcTable {
    procs: HashMap<i32, ProcInfo>,
    children: HashMap<i32, Vec<i32>>,
}

/// Direct children of `pid`, from `/proc/<pid>/task/<tid>/children`.
///
/// The file is **per thread**, not per process, so every task directory has to be read or a
/// child forked from a non-main thread is invisible. The kernel also documents it as not
/// atomic — the list can change while it is being read — which is fine for a 500 ms telemetry
/// scan and is the same race the walk already tolerates when a pid disappears before it is
/// stat'ed.
fn read_children(pid: i32) -> Vec<i32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for task in tasks.flatten() {
        let Ok(text) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        out.extend(
            text.split_ascii_whitespace()
                .filter_map(|t| t.parse::<i32>().ok()),
        );
    }
    out
}

impl ProcTable {
    /// Snapshot only the process trees rooted at `roots` (the tabs' shells).
    ///
    /// The scan used to call `all_processes()` and read `stat` + uid for **every** process on
    /// the machine — 500+ of them here, twice a second, to learn about the handful that belong
    /// to a tab. Descending from the roots instead reads the ones that actually matter, and
    /// the two accessors below are unchanged, so nothing downstream had to move.
    ///
    /// A process that daemonises away from its shell leaves the tree — exactly as it already
    /// left the global ppid map, since that was walked from the same roots.
    fn snapshot_for(roots: &[i32]) -> Self {
        let mut procs = HashMap::new();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut queue: VecDeque<i32> = roots.iter().copied().collect();
        let mut seen: HashSet<i32> = roots.iter().copied().collect();
        while let Some(pid) = queue.pop_front() {
            // A pid can vanish between being listed as a child and being read; that is normal
            // and simply means it is not in this snapshot.
            let Ok(proc) = procfs::process::Process::new(pid) else {
                continue;
            };
            let Ok(stat) = proc.stat() else { continue };
            let uid = proc.uid().ok();
            procs.insert(
                stat.pid,
                ProcInfo {
                    pid: stat.pid,
                    pgrp: stat.pgrp,
                    comm: stat.comm,
                    jiffies: stat.utime + stat.stime,
                    rss_pages: stat.rss,
                    uid,
                },
            );
            let kids = read_children(pid);
            for &k in &kids {
                if seen.insert(k) {
                    queue.push_back(k);
                }
            }
            if !kids.is_empty() {
                children.insert(pid, kids);
            }
        }
        Self { procs, children }
    }

    fn get(&self, pid: i32) -> Option<&ProcInfo> {
        self.procs.get(&pid)
    }

    /// Breadth-first descendants of `root`, excluding `root` itself.
    fn descendants(&self, root: i32) -> Vec<i32> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([root]);
        while let Some(pid) = queue.pop_front() {
            if let Some(kids) = self.children.get(&pid) {
                for &k in kids {
                    if seen.insert(k) {
                        out.push(k);
                        queue.push_back(k);
                    }
                }
            }
        }
        out
    }
}

fn read_cmdline(pid: i32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

const ELEVATION_TOOLS: &[&str] = &["sudo", "sudo-rs", "doas", "su", "pkexec", "run0"];
const REMOTE_TOOLS: &[&str] = &["ssh", "mosh-client", "mosh", "telnet", "et", "autossh"];

/// Programs whose presence in a tab means the shell has stepped into a local container.
/// `distrobox enter` execs the container manager, so `docker`/`podman` cover it either way;
/// the wrappers are listed too because they are what the user typed and they survive as the
/// parent. `/proc/<pid>/comm` is truncated at 15 bytes, which `distrobox-enter` exactly fits.
const CONTAINER_TOOLS: &[&str] = &[
    "distrobox-enter",
    "distrobox",
    "docker",
    "podman",
    "nerdctl",
    "toolbox",
    "lxc",
];

/// Extract the container name from a container-manager argv.
///
/// distrobox is the easy case and the one that matters here: it forwards the container name
/// as `--env=CONTAINER_ID=<name>`, which is unambiguous and does not depend on knowing which
/// flags of the underlying manager take a value. Observed on this machine:
/// `docker exec --interactive … --env=CONTAINER_ID=sedanos … sedanos /usr/bin/…`.
///
/// Failing that, fall back to the first positional after the subcommand, skipping flags. That
/// covers a hand-typed `podman exec -it myctr bash` and `distrobox enter myctr`.
pub fn container_name(argv: &[String]) -> Option<String> {
    // distrobox's own marker, wherever it appears in the line.
    for a in argv {
        if let Some(name) = a.strip_prefix("--env=CONTAINER_ID=")
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }
    /// Flags of `docker`/`podman exec` and `distrobox enter` that consume the next argument;
    /// without this list the value would be mistaken for the container name.
    const WITH_VALUE: &[&str] = &[
        "-e",
        "--env",
        "-u",
        "--user",
        "-w",
        "--workdir",
        "--detach-keys",
        "--env-file",
        "--name",
        "-n",
        "--additional-flags",
        "-a",
        "--home",
        "-H",
    ];
    // Subcommands that precede the container name; everything before one is manager chrome.
    const SUBCOMMANDS: &[&str] = &["exec", "enter", "run", "attach", "start"];
    let mut it = argv.iter().skip(1);
    let mut seen_subcommand = false;
    while let Some(a) = it.next() {
        if a.starts_with('-') {
            if WITH_VALUE.contains(&a.as_str()) {
                it.next();
            }
            continue;
        }
        if !seen_subcommand {
            // `distrobox enter x` has two words; `distrobox-enter x` has one.
            if SUBCOMMANDS.contains(&a.as_str()) {
                seen_subcommand = true;
            }
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Extract the remote destination from an ssh/mosh/telnet argv. Options that take a value
/// are skipped so `ssh -p 2222 -i key user@host cmd` → `host`.
pub fn remote_target(comm: &str, argv: &[String]) -> Option<String> {
    let args = argv.get(1..).unwrap_or(&[]);
    match comm {
        "mosh-client" => {
            // mosh-client <ip> <port>; the wrapper set MOSH_KEY; the ip is what we have.
            args.iter().find(|a| !a.starts_with('-')).cloned()
        }
        _ => {
            const WITH_VALUE: &[&str] = &[
                "-b", "-B", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O",
                "-o", "-p", "-P", "-Q", "-R", "-S", "-W", "-w",
            ];
            let mut iter = args.iter();
            while let Some(a) = iter.next() {
                if a == "--" {
                    return iter.next().map(|d| strip_destination(d));
                }
                if a.starts_with("--") {
                    // mosh long options (--ssh=..., --port=...); values are attached with '='.
                    continue;
                }
                if a.starts_with('-') && a.len() >= 2 {
                    if WITH_VALUE.contains(&&a[..2]) && a.len() == 2 {
                        iter.next();
                    }
                    continue;
                }
                return Some(strip_destination(a));
            }
            None
        }
    }
}

fn strip_destination(dest: &str) -> String {
    let d = dest.strip_prefix("ssh://").unwrap_or(dest);
    let d = d.rsplit('@').next().unwrap_or(d);
    // Drop an ssh:// style port and a trailing path.
    let d = d.split('/').next().unwrap_or(d);
    if d.starts_with('[') {
        // [ipv6]:port
        return d
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(d)
            .to_string();
    }
    match d.matches(':').count() {
        1 => d.split(':').next().unwrap_or(d).to_string(),
        _ => d.to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Scanner
// ---------------------------------------------------------------------------------------

struct GitCacheEntry {
    at: Instant,
    info: Option<GitInfo>,
}

struct Scanner {
    registry: Arc<ScanRegistry>,
    ctx: egui::Context,
    interval: Duration,
    home: PathBuf,
    cpu_prev: HashMap<TabId, (u64, Instant)>,
    git_cache: HashMap<PathBuf, GitCacheEntry>,
    page_size: u64,
    ticks_per_second: u64,
    /// Cores this process may actually use (respects cgroup/affinity limits), so the
    /// load meter has a denominator that does not move between samples.
    cpu_count: f32,
}

impl Scanner {
    fn run(mut self) {
        loop {
            thread::sleep(self.interval);
            let targets = self.registry.snapshot();
            if targets.is_empty() {
                self.cpu_prev.clear();
                continue;
            }
            let roots: Vec<i32> = targets.iter().map(|t| t.shell_pid).collect();
            let table = ProcTable::snapshot_for(&roots);
            let live: HashSet<TabId> = targets.iter().map(|t| t.id).collect();
            self.cpu_prev.retain(|id, _| live.contains(id));
            let mut changed = false;
            for target in &targets {
                changed |= self.inspect(target, &table);
            }
            if changed {
                self.ctx.request_repaint();
            }
        }
    }

    fn inspect(&mut self, target: &ScanTarget, table: &ProcTable) -> bool {
        let Some(shell) = table.get(target.shell_pid) else {
            return false; // shell gone; the session owner notices via try_wait
        };
        let descendants = table.descendants(target.shell_pid);

        // --- classification ---------------------------------------------------------
        let mut elevated = shell.uid == Some(0);
        let mut remote: Option<String> = None;
        let mut container: Option<String> = None;
        for pid in &descendants {
            let Some(p) = table.get(*pid) else { continue };
            if p.uid == Some(0) || ELEVATION_TOOLS.contains(&p.comm.as_str()) {
                elevated = true;
            }
            if remote.is_none() && REMOTE_TOOLS.contains(&p.comm.as_str()) {
                remote = Some(
                    remote_target(&p.comm, &read_cmdline(*pid)).unwrap_or_else(|| p.comm.clone()),
                );
            }
            if container.is_none() && CONTAINER_TOOLS.contains(&p.comm.as_str()) {
                container = container_name(&read_cmdline(*pid));
            }
        }
        // Most specific wins. Elevation stays first because it drives the red perimeter and
        // over-warning is the safe direction; remote outranks container because a command on
        // another host is further from this machine than one in a container on it.
        let category = if elevated {
            ProcessCategory::ElevatedRoot
        } else if let Some(target) = remote {
            ProcessCategory::RemoteSSH { target }
        } else if let Some(name) = container {
            ProcessCategory::Container { name }
        } else {
            ProcessCategory::Local
        };

        // --- foreground job -----------------------------------------------------------
        let fg_pgid = nix::unistd::tcgetpgrp(&target.master_fd)
            .map(|p| p.as_raw())
            .ok();
        let fg_leader: Option<&ProcInfo> = fg_pgid.filter(|&pg| pg != shell.pgrp).and_then(|pg| {
            table.get(pg).or_else(|| {
                descendants
                    .iter()
                    .filter_map(|pid| table.get(*pid))
                    .find(|p| p.pgrp == pg)
            })
        });

        // --- resource footprint over the foreground tree (or the idle shell) ----------
        let root = fg_leader.map(|p| p.pid).unwrap_or(target.shell_pid);
        let mut tree = vec![root];
        tree.extend(table.descendants(root));
        let (mut jiffies, mut rss_pages) = (0u64, 0u64);
        for pid in &tree {
            if let Some(p) = table.get(*pid) {
                jiffies += p.jiffies;
                rss_pages += p.rss_pages;
            }
        }
        let now = Instant::now();
        let cpu_pct = match self.cpu_prev.insert(target.id, (jiffies, now)) {
            Some((prev_j, prev_t)) => {
                let dt = now.duration_since(prev_t).as_secs_f32();
                if dt > 0.0 && jiffies >= prev_j {
                    (jiffies - prev_j) as f32 / (dt * self.ticks_per_second as f32) * 100.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        };

        // --- cwd / project / git ------------------------------------------------------
        let proc_cwd = std::fs::read_link(format!("/proc/{}/cwd", target.shell_pid)).ok();

        let (cwd, prev_root) = {
            let mut st = target.shared.state.lock();
            st.category = category;
            st.cpu_pct = cpu_pct;
            st.cpu_count = self.cpu_count;
            st.rss_bytes = rss_pages.saturating_mul(self.page_size);
            st.proc_count = tree.len();
            st.foreground = fg_leader.map(|p| ForegroundProc {
                comm: p.comm.clone(),
            });
            if proc_cwd.is_some() {
                st.proc_cwd = proc_cwd;
            }
            (st.cwd().map(Path::to_path_buf), st.project_root.clone())
        };
        let root = cwd
            .as_deref()
            .and_then(|c| detect_project_root(c, &self.home));
        let git = root.as_deref().and_then(|r| self.git_info(r));

        let mut st = target.shared.state.lock();
        st.project_root = root.clone();
        st.git = git;

        // --- command lifecycle fallback (only without OSC 133) ------------------------
        if !st.osc133_seen {
            match (st.running.is_some(), fg_leader) {
                (false, Some(p)) => st.start_command(Some(p.comm.clone()), RunSource::ProcScan),
                (true, None) => st.finish_command(None),
                _ => {}
            }
        }
        if let (Some(run), Some(p)) = (st.running.as_mut(), fg_leader)
            && run.command_name.is_empty()
        {
            run.command_name = p.comm.clone();
        }
        let _ = prev_root;
        true
    }

    fn git_info(&mut self, root: &Path) -> Option<GitInfo> {
        const TTL: Duration = Duration::from_secs(5);
        if let Some(entry) = self.git_cache.get(root)
            && entry.at.elapsed() < TTL
        {
            return entry.info.clone();
        }
        let info = git_branch(root).map(|branch| GitInfo {
            branch,
            dirty: git_dirty(root).unwrap_or(false),
        });
        self.git_cache.insert(
            root.to_path_buf(),
            GitCacheEntry {
                at: Instant::now(),
                info: info.clone(),
            },
        );
        // Keep the cache bounded to roots seen recently.
        if self.git_cache.len() > 64 {
            let cutoff = Instant::now() - Duration::from_secs(300);
            self.git_cache.retain(|_, e| e.at > cutoff);
        }
        info
    }
}

/// Branch name (or short SHA when detached) read straight from `.git/HEAD`; handles
/// worktrees whose `.git` is a `gitdir:` pointer file.
pub fn git_branch(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        let p = PathBuf::from(target);
        if p.is_absolute() { p } else { root.join(p) }
    } else {
        return None;
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref: ") {
        Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string())
    } else {
        Some(head.chars().take(8).collect())
    }
}

/// `git diff-index --quiet HEAD` exits 1 when the index or worktree differs from HEAD.
/// Bounded to 1.5 s so a huge repository can never stall the scanner.
fn git_dirty(root: &Path) -> Option<bool> {
    let mut child = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["--no-optional-locks", "diff-index", "--quiet", "HEAD", "--"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.code() == Some(1)),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn ssh_destination_parsing() {
        assert_eq!(
            remote_target("ssh", &argv("ssh user@server.domain.com")),
            Some("server.domain.com".into())
        );
        assert_eq!(
            remote_target("ssh", &argv("ssh -p 2222 -i ~/.ssh/id host")),
            Some("host".into())
        );
        assert_eq!(
            remote_target("ssh", &argv("ssh -4 -A -tt root@10.0.0.5 uptime")),
            Some("10.0.0.5".into())
        );
        assert_eq!(
            remote_target("ssh", &argv("ssh -J bastion prod-db-01")),
            Some("prod-db-01".into())
        );
        assert_eq!(
            remote_target("ssh", &argv("ssh ssh://me@box:2200/")),
            Some("box".into())
        );
        assert_eq!(remote_target("ssh", &argv("ssh")), None);
    }

    #[test]
    fn mosh_and_telnet() {
        assert_eq!(
            remote_target("mosh", &argv("mosh --ssh=ssh -p 60001 me@edge")),
            Some("edge".into())
        );
        assert_eq!(
            remote_target("mosh-client", &argv("mosh-client 192.168.1.9 60001")),
            Some("192.168.1.9".into())
        );
        assert_eq!(
            remote_target("telnet", &argv("telnet bbs.example.org 23")),
            Some("bbs.example.org".into())
        );
    }

    #[test]
    fn container_name_from_the_distrobox_marker() {
        // Real argv shape observed on this machine: distrobox forwards the container name as
        // --env=CONTAINER_ID, which is unambiguous wherever it lands in a very long line.
        let argv: Vec<String> = [
            "docker",
            "exec",
            "--interactive",
            "--user=rayben",
            "--workdir=/run/host/home/rayben",
            "--env=PWD=/run/host/home/rayben",
            "--env=CONTAINER_ID=sedanos",
            "--env=LANG=en_US.UTF-8",
            "sedanos",
            "/usr/bin/microsoft-edge-stable",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(container_name(&argv).as_deref(), Some("sedanos"));
    }

    #[test]
    fn container_name_falls_back_to_the_positional_after_the_subcommand() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            container_name(&argv(&["podman", "exec", "-it", "myctr", "bash"])).as_deref(),
            Some("myctr")
        );
        assert_eq!(
            container_name(&argv(&["distrobox", "enter", "arch"])).as_deref(),
            Some("arch")
        );
        // Flags that take a value must not be mistaken for the name.
        assert_eq!(
            container_name(&argv(&[
                "docker", "exec", "-u", "root", "-w", "/tmp", "-e", "FOO=1", "web", "sh"
            ]))
            .as_deref(),
            Some("web")
        );
        // `--env FOO=bar` split form, then the name.
        assert_eq!(
            container_name(&argv(&["podman", "exec", "--env", "A=b", "db", "psql"])).as_deref(),
            Some("db")
        );
    }

    #[test]
    fn container_name_is_none_when_there_is_nothing_to_name() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(container_name(&argv(&["docker", "ps"])), None);
        assert_eq!(container_name(&argv(&["docker"])), None);
        // `exec` with no positional left after the flags.
        assert_eq!(container_name(&argv(&["docker", "exec", "-it"])), None);
        // An empty marker must not win over the fallback.
        assert_eq!(
            container_name(&argv(&[
                "docker",
                "exec",
                "--env=CONTAINER_ID=",
                "real",
                "sh"
            ]))
            .as_deref(),
            Some("real")
        );
    }

    #[test]
    fn the_targeted_walk_finds_a_real_process_tree() {
        // Build a small tree under this test process and check the walk reproduces it. This is
        // what `all_processes()` used to give for free, so it is the property that has to hold
        // after the change — including a grandchild, which is what makes it a *walk*.
        let mut child = std::process::Command::new("bash")
            .args(["-c", "sleep 5 & sleep 5"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn probe tree");
        let root = std::process::id() as i32;
        let child_pid = child.id() as i32;
        // Give bash a moment to fork its own children.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let table = ProcTable::snapshot_for(&[root]);
        assert!(
            table.get(root).is_some(),
            "the root itself must be in the table"
        );
        let desc = table.descendants(root);
        assert!(desc.contains(&child_pid), "direct child missing: {desc:?}");
        // The `sleep`s are children of the bash we spawned, so finding one proves the walk
        // recurses rather than reading a single level of `children`.
        assert!(
            desc.iter()
                .filter_map(|p| table.get(*p))
                .any(|p| p.comm == "sleep"),
            "no grandchild found in {desc:?}"
        );
        // And it stays a *subtree*: pid 1 is not under us.
        assert!(!desc.contains(&1));

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn the_walk_survives_a_root_that_no_longer_exists() {
        // A shell can exit between the registry snapshot and the scan; that must be an empty
        // table, not a panic.
        let table = ProcTable::snapshot_for(&[i32::MAX]);
        assert!(table.get(i32::MAX).is_none());
        assert!(table.descendants(i32::MAX).is_empty());
    }

    #[test]
    fn descendants_walk() {
        let mut children = HashMap::new();
        children.insert(1, vec![10, 11]);
        children.insert(10, vec![100]);
        let table = ProcTable {
            procs: HashMap::new(),
            children,
        };
        let mut d = table.descendants(1);
        d.sort();
        assert_eq!(d, vec![10, 11, 100]);
    }
}

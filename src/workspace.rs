//! The tab tree: buckets (Local / SSH-Remote / Elevated / Ad-hoc) → groups → tabs.
//! Grouping is recomputed from live session state every frame; a manual override pins a tab
//! to a specific group key. Also hosts project-root detection used by the scanner.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::session::{ProcessCategory, SessionState, TabId, TabKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bucket {
    Local,
    Remote,
    Container,
    Elevated,
    Ephemeral,
}

impl Bucket {
    pub fn key(self) -> &'static str {
        match self {
            Bucket::Local => "bucket:local",
            Bucket::Remote => "bucket:remote",
            Bucket::Container => "bucket:container",
            Bucket::Elevated => "bucket:elevated",
            Bucket::Ephemeral => "bucket:ephemeral",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Bucket::Local => "LOCAL",
            Bucket::Remote => "SSH / REMOTE",
            Bucket::Container => "CONTAINERS",
            Bucket::Elevated => "ELEVATED",
            Bucket::Ephemeral => "AD-HOC",
        }
    }

    pub fn from_group_key(key: &str) -> Bucket {
        if key.starts_with("remote:") {
            Bucket::Remote
        } else if key.starts_with("container:") {
            Bucket::Container
        } else if key == "elevated" || key.starts_with("elevated:") {
            Bucket::Elevated
        } else if key == "ephemeral" {
            Bucket::Ephemeral
        } else {
            Bucket::Local
        }
    }
}

#[derive(Clone, Debug)]
pub struct GroupNode {
    pub key: String,
    pub name: String,
    pub tabs: Vec<TabId>,
}

#[derive(Clone, Debug)]
pub struct BucketNode {
    pub bucket: Bucket,
    pub groups: Vec<GroupNode>,
}

#[derive(Clone, Debug, Default)]
pub struct TabTree {
    pub buckets: Vec<BucketNode>,
}

pub struct TabInput<'a> {
    pub id: TabId,
    pub kind: TabKind,
    pub group_override: Option<&'a str>,
    pub state: &'a SessionState,
}

/// The group a tab belongs to when nothing pins it elsewhere.
pub fn natural_group_key(kind: TabKind, state: &SessionState, home: &Path) -> String {
    if kind == TabKind::Ephemeral {
        return "ephemeral".to_string();
    }
    match &state.category {
        ProcessCategory::ElevatedRoot => "elevated".to_string(),
        ProcessCategory::RemoteSSH { target } => format!("remote:{target}"),
        ProcessCategory::Container { name } => format!("container:{name}"),
        ProcessCategory::Local => match &state.project_root {
            Some(root) => format!("local:{}", root.display()),
            None => match state.cwd() {
                Some(cwd) if cwd.starts_with(home) => "local:~".to_string(),
                Some(_) => "local:/".to_string(),
                None => "local:~".to_string(),
            },
        },
    }
}

pub fn group_name_from_key(key: &str) -> String {
    if let Some(rest) = key.strip_prefix("local:") {
        return match rest {
            "~" | "/" => rest.to_string(),
            path => Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| path.to_string()),
        };
    }
    if let Some(host) = key.strip_prefix("remote:") {
        return host.to_string();
    }
    if let Some(name) = key.strip_prefix("container:") {
        return name.to_string();
    }
    match key {
        "elevated" => "root".to_string(),
        "ephemeral" => "scratch".to_string(),
        other => other.to_string(),
    }
}

pub fn build_tree(tabs: &[TabInput<'_>], home: &Path) -> TabTree {
    // bucket → group key → (name, tabs)
    let mut buckets: BTreeMap<Bucket, BTreeMap<String, GroupNode>> = BTreeMap::new();
    for t in tabs {
        let key = match t.group_override {
            Some(k) => k.to_string(),
            None => natural_group_key(t.kind, t.state, home),
        };
        let bucket = Bucket::from_group_key(&key);
        let group = buckets
            .entry(bucket)
            .or_default()
            .entry(key.clone())
            .or_insert_with(|| GroupNode {
                name: group_name_from_key(&key),
                key,
                tabs: Vec::new(),
            });
        group.tabs.push(t.id);
    }
    let mut tree = TabTree::default();
    for (bucket, groups) in buckets {
        let mut groups: Vec<GroupNode> = groups.into_values().collect();
        // "~" first, then alphabetical; stable across frames because keys are stable.
        groups.sort_by(|a, b| {
            let rank = |g: &GroupNode| if g.key == "local:~" { 0 } else { 1 };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.key.cmp(&b.key))
        });
        tree.buckets.push(BucketNode { bucket, groups });
    }
    tree
}

impl TabTree {
    /// Every tab in display order (bucket order, group order, insertion order).
    pub fn ordered_tabs(&self) -> Vec<TabId> {
        self.buckets
            .iter()
            .flat_map(|b| b.groups.iter().flat_map(|g| g.tabs.iter().copied()))
            .collect()
    }

    pub fn group_keys(&self) -> Vec<String> {
        self.buckets
            .iter()
            .flat_map(|b| b.groups.iter().map(|g| g.key.clone()))
            .collect()
    }

    pub fn group_of(&self, id: TabId) -> Option<(&BucketNode, &GroupNode)> {
        for b in &self.buckets {
            for g in &b.groups {
                if g.tabs.contains(&id) {
                    return Some((b, g));
                }
            }
        }
        None
    }
}

const GIT_MARKER: &str = ".git";
const PROJECT_MARKERS: &[&str] = &[
    "Cargo.toml",
    "go.mod",
    "package.json",
    "pyproject.toml",
    "flake.nix",
];

/// Walk up from `cwd`. The nearest `.git` wins (nested repos are separate projects); failing
/// that, the *outermost* build-system marker (so a Cargo workspace member groups under the
/// workspace). `$HOME` and `/` themselves are never treated as projects.
pub fn detect_project_root(cwd: &Path, home: &Path) -> Option<PathBuf> {
    let mut outermost_marker: Option<PathBuf> = None;
    let mut cur: Option<&Path> = Some(cwd);
    while let Some(dir) = cur {
        if dir == home || dir.parent().is_none() {
            break;
        }
        if dir.join(GIT_MARKER).exists() {
            return Some(dir.to_path_buf());
        }
        if PROJECT_MARKERS.iter().any(|m| dir.join(m).is_file()) {
            outermost_marker = Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    outermost_marker
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(cat: ProcessCategory, root: Option<&str>, cwd: Option<&str>) -> SessionState {
        SessionState {
            category: cat,
            project_root: root.map(PathBuf::from),
            proc_cwd: cwd.map(PathBuf::from),
            ..Default::default()
        }
    }

    #[test]
    fn natural_keys() {
        let home = Path::new("/home/me");
        assert_eq!(
            natural_group_key(TabKind::Ephemeral, &SessionState::default(), home),
            "ephemeral"
        );
        assert_eq!(
            natural_group_key(
                TabKind::Normal,
                &state(ProcessCategory::ElevatedRoot, None, None),
                home
            ),
            "elevated"
        );
        assert_eq!(
            natural_group_key(
                TabKind::Normal,
                &state(
                    ProcessCategory::RemoteSSH {
                        target: "db01".into()
                    },
                    None,
                    None
                ),
                home
            ),
            "remote:db01"
        );
        assert_eq!(
            natural_group_key(
                TabKind::Normal,
                &state(ProcessCategory::Local, Some("/home/me/src/verterm"), None),
                home
            ),
            "local:/home/me/src/verterm"
        );
        assert_eq!(
            natural_group_key(
                TabKind::Normal,
                &state(ProcessCategory::Local, None, Some("/home/me/docs")),
                home
            ),
            "local:~"
        );
        assert_eq!(
            natural_group_key(
                TabKind::Normal,
                &state(ProcessCategory::Local, None, Some("/srv/www")),
                home
            ),
            "local:/"
        );
    }

    #[test]
    fn tree_order_and_override() {
        let home = Path::new("/home/me");
        let s1 = state(ProcessCategory::Local, Some("/home/me/src/zeta"), None);
        let s2 = state(ProcessCategory::Local, None, Some("/home/me"));
        let s3 = state(
            ProcessCategory::RemoteSSH {
                target: "box".into(),
            },
            None,
            None,
        );
        let s4 = state(ProcessCategory::Local, Some("/home/me/src/alpha"), None);
        let tabs = vec![
            TabInput {
                id: 1,
                kind: TabKind::Normal,
                group_override: None,
                state: &s1,
            },
            TabInput {
                id: 2,
                kind: TabKind::Normal,
                group_override: None,
                state: &s2,
            },
            TabInput {
                id: 3,
                kind: TabKind::Normal,
                group_override: None,
                state: &s3,
            },
            TabInput {
                id: 4,
                kind: TabKind::Normal,
                group_override: Some("remote:box"),
                state: &s4,
            },
        ];
        let tree = build_tree(&tabs, home);
        assert_eq!(tree.buckets.len(), 2);
        assert_eq!(tree.buckets[0].bucket, Bucket::Local);
        let names: Vec<_> = tree.buckets[0]
            .groups
            .iter()
            .map(|g| g.name.as_str())
            .collect();
        assert_eq!(names, vec!["~", "zeta"]);
        assert_eq!(tree.buckets[1].groups[0].tabs, vec![3, 4]);
        assert_eq!(tree.ordered_tabs(), vec![2, 1, 3, 4]);
    }

    #[test]
    fn project_root_detection() {
        let tmp = std::env::temp_dir().join(format!("verterm-ws-{}", std::process::id()));
        let home = tmp.join("home");
        let repo = home.join("src").join("proj");
        let member = repo.join("crates").join("a");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "").unwrap();
        std::fs::write(member.join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_project_root(&member, &home), Some(repo.clone()));
        std::fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(
            detect_project_root(&member, &home),
            Some(repo.clone()),
            "outermost Cargo.toml"
        );
        assert_eq!(detect_project_root(&home.join("docs"), &home), None);
        std::fs::remove_dir_all(&tmp).ok();
    }
    #[test]
    fn a_container_session_gets_its_own_bucket_and_group() {
        let st = state(
            ProcessCategory::Container {
                name: "sedanos".into(),
            },
            None,
            Some("/home/me"),
        );
        let key = natural_group_key(TabKind::Normal, &st, Path::new("/home/me"));
        assert_eq!(key, "container:sedanos");
        // The bucket must be derivable from the key alone, or a manual override could not
        // move a tab into (or out of) the container bucket.
        assert_eq!(Bucket::from_group_key(&key), Bucket::Container);
        assert_eq!(group_name_from_key(&key), "sedanos");
        assert_eq!(Bucket::Container.label(), "CONTAINERS");
        // A cwd under $HOME must not pull it back into the local "~" group.
        assert_ne!(key, "local:~");
    }
}

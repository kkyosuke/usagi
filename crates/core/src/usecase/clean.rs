//! Pure orphan-resource classification for `usagi clean`.
//!
//! Discovery and deletion are real-IO concerns of the composition root.  This
//! module owns the important decision instead: only resources in usagi's
//! managed namespace which are absent from the daemon lifecycle document are
//! candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::infrastructure::paths::{SESSIONS_DIR, STATE_DIR};

/// One registered workspace entry and whether its path still exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredWorkspace {
    pub path: PathBuf,
    pub exists: bool,
}

/// One daemon-owned workspace-state subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonWorkspaceData {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub root_exists: bool,
    /// `None` means the lifecycle document is missing or unreadable. Such a
    /// subtree is never used to classify Git resources as orphaned, and its
    /// data is retained for repair rather than treated as disposable.
    pub sessions: Option<BTreeSet<String>>,
}

/// One worktree reported by Git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub dirty: bool,
}

/// One local branch in a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedBranch {
    pub name: String,
    pub merged: bool,
}

/// Git resources observed for one repository root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInventory {
    pub root: PathBuf,
    pub worktrees: Vec<ObservedWorktree>,
    pub branches: Vec<ObservedBranch>,
}

/// All observations needed for one cleanup plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanInventory {
    pub registered: Vec<RegisteredWorkspace>,
    pub daemon_data: Vec<DaemonWorkspaceData>,
    pub repositories: Vec<RepositoryInventory>,
}

/// A provably unlinked resource. Ordering is significant: worktrees precede
/// their branches, and registry entries precede state data only for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanCandidate {
    Workspace {
        path: PathBuf,
    },
    Data {
        root: PathBuf,
        dir: PathBuf,
    },
    Worktree {
        root: PathBuf,
        path: PathBuf,
        requires_force: bool,
    },
    Branch {
        root: PathBuf,
        name: String,
        requires_force: bool,
    },
}

impl CleanCandidate {
    /// Whether applying this candidate may discard user changes.
    #[must_use]
    pub const fn requires_force(&self) -> bool {
        match self {
            Self::Worktree { requires_force, .. } | Self::Branch { requires_force, .. } => {
                *requires_force
            }
            Self::Workspace { .. } | Self::Data { .. } => false,
        }
    }
}

/// Build a stable cleanup plan from observations.
#[must_use]
pub fn plan(inventory: &CleanInventory) -> Vec<CleanCandidate> {
    let mut candidates = Vec::new();
    let lifecycle = inventory
        .daemon_data
        .iter()
        .filter_map(|data| {
            data.sessions
                .as_ref()
                .map(|sessions| (&data.root, sessions))
        })
        .collect::<BTreeMap<_, _>>();

    let mut registered = inventory.registered.iter().collect::<Vec<_>>();
    registered.sort_by(|left, right| left.path.cmp(&right.path));
    candidates.extend(
        registered
            .into_iter()
            .filter(|workspace| !workspace.exists)
            .map(|workspace| CleanCandidate::Workspace {
                path: workspace.path.clone(),
            }),
    );

    let mut data = inventory.daemon_data.iter().collect::<Vec<_>>();
    data.sort_by(|left, right| left.root.cmp(&right.root));
    candidates.extend(
        data.into_iter()
            .filter(|data| !data.root_exists && data.sessions.is_some())
            .map(|data| CleanCandidate::Data {
                root: data.root.clone(),
                dir: data.dir.clone(),
            }),
    );

    let mut repositories = inventory.repositories.iter().collect::<Vec<_>>();
    repositories.sort_by(|left, right| left.root.cmp(&right.root));
    for repository in repositories {
        let Some(sessions) = lifecycle.get(&repository.root) else {
            continue;
        };
        let expected_parent = repository.root.join(STATE_DIR).join(SESSIONS_DIR);
        let mut worktrees = repository.worktrees.iter().collect::<Vec<_>>();
        worktrees.sort_by(|left, right| left.path.cmp(&right.path));
        for worktree in worktrees {
            let Some(name) = managed_worktree_name(&expected_parent, worktree) else {
                continue;
            };
            if !sessions.contains(name) {
                candidates.push(CleanCandidate::Worktree {
                    root: repository.root.clone(),
                    path: worktree.path.clone(),
                    requires_force: worktree.dirty,
                });
            }
        }

        let mut branches = repository.branches.iter().collect::<Vec<_>>();
        branches.sort_by(|left, right| left.name.cmp(&right.name));
        for branch in branches {
            let Some(name) = branch.name.strip_prefix("usagi/") else {
                continue;
            };
            if !sessions.contains(name) {
                candidates.push(CleanCandidate::Branch {
                    root: repository.root.clone(),
                    name: branch.name.clone(),
                    requires_force: !branch.merged,
                });
            }
        }
    }
    candidates
}

fn managed_worktree_name<'a>(
    expected_parent: &Path,
    worktree: &'a ObservedWorktree,
) -> Option<&'a str> {
    if worktree.path.parent()? != expected_parent {
        return None;
    }
    let name = worktree.path.file_name()?.to_str()?;
    match worktree.branch.as_deref() {
        Some(branch) if branch == format!("usagi/{name}") => Some(name),
        // A detached worktree at the exact managed path is still owned by the
        // namespace, but deleting it always requires force (represented by its
        // dirty observation at discovery time).
        None => Some(name),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn plans_only_unlinked_managed_resources_in_stable_order() {
        let inventory = CleanInventory {
            registered: vec![
                RegisteredWorkspace {
                    path: "/z".into(),
                    exists: false,
                },
                RegisteredWorkspace {
                    path: "/a".into(),
                    exists: true,
                },
            ],
            daemon_data: vec![
                DaemonWorkspaceData {
                    root: "/gone".into(),
                    dir: "/data/gone".into(),
                    root_exists: false,
                    sessions: Some(BTreeSet::new()),
                },
                DaemonWorkspaceData {
                    root: "/a".into(),
                    dir: "/data/a".into(),
                    root_exists: true,
                    sessions: Some(set(&["live"])),
                },
            ],
            repositories: vec![RepositoryInventory {
                root: "/a".into(),
                worktrees: vec![
                    ObservedWorktree {
                        path: "/a/.usagi/sessions/stale".into(),
                        branch: Some("usagi/stale".into()),
                        dirty: true,
                    },
                    ObservedWorktree {
                        path: "/a/.usagi/sessions/live".into(),
                        branch: Some("usagi/live".into()),
                        dirty: false,
                    },
                    ObservedWorktree {
                        path: "/elsewhere".into(),
                        branch: Some("usagi/other".into()),
                        dirty: false,
                    },
                ],
                branches: vec![
                    ObservedBranch {
                        name: "main".into(),
                        merged: true,
                    },
                    ObservedBranch {
                        name: "usagi/live".into(),
                        merged: true,
                    },
                    ObservedBranch {
                        name: "usagi/stale".into(),
                        merged: false,
                    },
                ],
            }],
        };
        assert_eq!(
            plan(&inventory),
            vec![
                CleanCandidate::Workspace { path: "/z".into() },
                CleanCandidate::Data {
                    root: "/gone".into(),
                    dir: "/data/gone".into()
                },
                CleanCandidate::Worktree {
                    root: "/a".into(),
                    path: "/a/.usagi/sessions/stale".into(),
                    requires_force: true
                },
                CleanCandidate::Branch {
                    root: "/a".into(),
                    name: "usagi/stale".into(),
                    requires_force: true
                },
            ]
        );
    }

    #[test]
    fn refuses_to_infer_git_orphans_without_readable_lifecycle_state() {
        let inventory = CleanInventory {
            daemon_data: vec![DaemonWorkspaceData {
                root: "/repo".into(),
                dir: "/data/repo".into(),
                root_exists: false,
                sessions: None,
            }],
            repositories: vec![RepositoryInventory {
                root: "/repo".into(),
                worktrees: vec![ObservedWorktree {
                    path: "/repo/.usagi/sessions/x".into(),
                    branch: Some("usagi/x".into()),
                    dirty: false,
                }],
                branches: vec![ObservedBranch {
                    name: "usagi/x".into(),
                    merged: true,
                }],
            }],
            ..CleanInventory::default()
        };
        assert!(plan(&inventory).is_empty());
    }

    #[test]
    fn ignores_branch_mismatches_and_marks_unmerged_unchecked_branches_force_only() {
        let inventory = CleanInventory {
            daemon_data: vec![DaemonWorkspaceData {
                root: "/repo".into(),
                dir: "/data/repo".into(),
                root_exists: true,
                sessions: Some(BTreeSet::new()),
            }],
            repositories: vec![RepositoryInventory {
                root: "/repo".into(),
                worktrees: vec![ObservedWorktree {
                    path: "/repo/.usagi/sessions/x".into(),
                    branch: Some("feature/x".into()),
                    dirty: false,
                }],
                branches: vec![ObservedBranch {
                    name: "usagi/y".into(),
                    merged: false,
                }],
            }],
            ..CleanInventory::default()
        };
        assert_eq!(
            plan(&inventory),
            vec![CleanCandidate::Branch {
                root: "/repo".into(),
                name: "usagi/y".into(),
                requires_force: true
            }]
        );
    }

    #[test]
    fn sorts_repositories_and_accepts_detached_managed_worktrees() {
        let inventory = CleanInventory {
            daemon_data: vec![
                DaemonWorkspaceData {
                    root: "/b".into(),
                    dir: "/data/b".into(),
                    root_exists: true,
                    sessions: Some(BTreeSet::new()),
                },
                DaemonWorkspaceData {
                    root: "/a".into(),
                    dir: "/data/a".into(),
                    root_exists: true,
                    sessions: Some(BTreeSet::new()),
                },
            ],
            repositories: vec![
                RepositoryInventory {
                    root: "/b".into(),
                    worktrees: vec![ObservedWorktree {
                        path: "/b/.usagi/sessions/detached".into(),
                        branch: None,
                        dirty: true,
                    }],
                    branches: Vec::new(),
                },
                RepositoryInventory {
                    root: "/a".into(),
                    worktrees: Vec::new(),
                    branches: Vec::new(),
                },
            ],
            ..CleanInventory::default()
        };

        assert_eq!(
            plan(&inventory),
            vec![CleanCandidate::Worktree {
                root: "/b".into(),
                path: "/b/.usagi/sessions/detached".into(),
                requires_force: true,
            }]
        );
    }

    #[test]
    fn candidate_force_flag_matches_only_destructive_git_cases() {
        assert!(!CleanCandidate::Workspace { path: "/x".into() }.requires_force());
        assert!(
            !CleanCandidate::Data {
                root: "/x".into(),
                dir: "/d".into()
            }
            .requires_force()
        );
        assert!(
            CleanCandidate::Worktree {
                root: "/x".into(),
                path: "/w".into(),
                requires_force: true
            }
            .requires_force()
        );
        assert!(
            CleanCandidate::Branch {
                root: "/x".into(),
                name: "usagi/x".into(),
                requires_force: true
            }
            .requires_force()
        );
    }
}

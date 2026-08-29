//! Pure orphan-resource classification for `usagi clean`.
//!
//! Discovery and deletion are real-IO concerns of the composition root.  This
//! module owns the important decision instead: only resources in usagi's
//! managed namespace which are absent from the daemon lifecycle document are
//! candidates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::infrastructure::git::{GitRunner, list_worktrees};
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

/// What a `usagi` helper process was started to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HelperRole {
    /// `usagi daemon serve`. Owns PTYs and workspace state while it runs, so
    /// reaping one is destructive even when it is residue.
    Daemon,
    /// `usagi daemon bootstrap-broker`. Exists only so a sandboxed client can
    /// cold-start a daemon; it owns nothing a user can lose.
    Broker,
}

/// One `usagi` helper process observed on this host under this user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedProcess {
    pub pid: u32,
    pub role: HelperRole,
    /// The absolute executable path the process runs. The daemon and the broker
    /// are always spawned from a canonical path, so a bare or relative one is
    /// not a process this plan is entitled to judge.
    pub executable: PathBuf,
    /// Whether that executable still exists on disk.
    pub executable_exists: bool,
    /// The process-start identity observed with the pid. Carried through to the
    /// candidate so the removal can re-verify it immediately before signalling
    /// and never act on a pid the OS has since handed to someone else.
    pub start_identity: String,
    /// Whether this data home still accounts for the process: its lifecycle
    /// record, its generation registry, or a broker record names this pid.
    pub accounted: bool,
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
    pub processes: Vec<ObservedProcess>,
}

/// Observe the managed worktree and branch namespace for one repository.
///
/// Both the standalone CLI and the running daemon use this exact inventory
/// path, so cleanup classification cannot drift between the offline and
/// daemon-owned surfaces.
///
/// # Errors
///
/// Returns the first Git observation failure. A path outside a Git worktree is
/// represented as `Ok(None)` rather than an empty, cleanup-authorizing
/// inventory.
pub fn observe_repository(
    git: &dyn GitRunner,
    root: &Path,
) -> anyhow::Result<Option<RepositoryInventory>> {
    let probe = git.run(root, &["rev-parse", "--is-inside-work-tree"])?;
    if !probe.success || probe.stdout.trim() != "true" {
        return Ok(None);
    }
    let expected_parent = root.join(STATE_DIR).join(SESSIONS_DIR);
    let mut worktrees = Vec::new();
    for worktree in list_worktrees(git, root)? {
        if worktree.path.parent() != Some(expected_parent.as_path()) {
            continue;
        }
        let status = git.run(&worktree.path, &["status", "--porcelain"])?;
        worktrees.push(ObservedWorktree {
            path: worktree.path,
            dirty: !status.success || !status.stdout.trim().is_empty() || worktree.branch.is_none(),
            branch: worktree.branch,
        });
    }
    let refs = git.run(
        root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/usagi/",
        ],
    )?;
    if !refs.success {
        anyhow::bail!("git branch inventory failed: {}", refs.stderr.trim());
    }
    let mut branches = Vec::new();
    for name in refs
        .stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let merged = git
            .run(root, &["merge-base", "--is-ancestor", name, "HEAD"])?
            .success;
        branches.push(ObservedBranch {
            name: name.to_owned(),
            merged,
        });
    }
    Ok(Some(RepositoryInventory {
        root: root.to_path_buf(),
        worktrees,
        branches,
    }))
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
    /// A `usagi` helper process left behind by a build that no longer exists.
    Process {
        pid: u32,
        role: HelperRole,
        executable: PathBuf,
        start_identity: String,
        requires_force: bool,
    },
}

impl CleanCandidate {
    /// Whether applying this candidate may discard user changes.
    #[must_use]
    pub const fn requires_force(&self) -> bool {
        match self {
            Self::Worktree { requires_force, .. }
            | Self::Branch { requires_force, .. }
            | Self::Process { requires_force, .. } => *requires_force,
            Self::Workspace { .. } | Self::Data { .. } => false,
        }
    }
}

/// Build a stable cleanup plan from observations.
#[must_use]
pub fn plan(inventory: &CleanInventory) -> Vec<CleanCandidate> {
    let mut candidates = Vec::new();

    // Processes come first. A leaked daemon holds the worktree it was started
    // in, so reaping it is what lets the Git steps below succeed rather than
    // fail against a busy checkout.
    let mut processes = inventory.processes.iter().collect::<Vec<_>>();
    processes.sort_by_key(|process| process.pid);
    candidates.extend(
        processes
            .into_iter()
            .filter(|process| residue(process))
            .map(|process| CleanCandidate::Process {
                pid: process.pid,
                role: process.role,
                executable: process.executable.clone(),
                start_identity: process.start_identity.clone(),
                // A daemon may still own PTYs whose output nobody has read yet, so
                // ending one is a deliberate act. A broker owns nothing.
                requires_force: process.role == HelperRole::Daemon,
            }),
    );

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
        // An empty session list is absence of evidence, not evidence of absence.
        // It makes *every* managed worktree and branch of the workspace look
        // unlinked at once, which is the one shape of this plan that can remove
        // a whole workspace's work in a single run — and the states that produce
        // it (a workspace the daemon released, a document written before its
        // sessions were recorded) say nothing about the worktrees on disk. It
        // still authorises removal, because a workspace whose sessions really
        // are all gone can leak worktrees, but only as a deliberate act.
        let unproven = sessions.is_empty();
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
                    requires_force: worktree.dirty || unproven,
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
                    requires_force: !branch.merged || unproven,
                });
            }
        }
    }
    candidates
}

/// Whether a helper process is residue this plan may end.
///
/// Both conditions are required. A missing executable proves the process cannot
/// belong to any current installation — it can neither be restarted nor
/// upgraded, and nothing will ever address it again. Being unaccounted for
/// proves this data home is not relying on it. Either one alone is ordinary: a
/// live daemon's executable exists, and a healthy daemon started from another
/// data home is simply not ours to judge.
fn residue(process: &ObservedProcess) -> bool {
    !process.executable_exists
        && !process.accounted
        && process.executable.is_absolute()
        && !process.start_identity.is_empty()
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
    use crate::infrastructure::git::testkit::{FakeGit, fail, ok};

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn process(
        pid: u32,
        role: HelperRole,
        exe: &str,
        exists: bool,
        accounted: bool,
    ) -> ObservedProcess {
        ObservedProcess {
            pid,
            role,
            executable: exe.into(),
            executable_exists: exists,
            start_identity: format!("identity-{pid}"),
            accounted,
        }
    }

    #[test]
    fn repository_observation_classifies_managed_git_resources() {
        let worktrees = "worktree /repo\nHEAD root\nbranch refs/heads/main\n\
                         \nworktree /elsewhere\nHEAD other\nbranch refs/heads/usagi/elsewhere\n\
                         \nworktree /repo/.usagi/sessions/clean\nHEAD clean\nbranch refs/heads/usagi/clean\n\
                         \nworktree /repo/.usagi/sessions/detached\nHEAD detached\ndetached\n";
        let git = FakeGit::new(vec![
            ok("true\n"),
            ok(worktrees),
            ok(""),
            fail("status unavailable"),
            ok("usagi/clean\n\nusagi/detached\n"),
            ok(""),
            fail("not merged"),
        ]);

        assert_eq!(
            observe_repository(&git, Path::new("/repo")).unwrap(),
            Some(RepositoryInventory {
                root: "/repo".into(),
                worktrees: vec![
                    ObservedWorktree {
                        path: "/repo/.usagi/sessions/clean".into(),
                        dirty: false,
                        branch: Some("usagi/clean".into()),
                    },
                    ObservedWorktree {
                        path: "/repo/.usagi/sessions/detached".into(),
                        dirty: true,
                        branch: None,
                    },
                ],
                branches: vec![
                    ObservedBranch {
                        name: "usagi/clean".into(),
                        merged: true,
                    },
                    ObservedBranch {
                        name: "usagi/detached".into(),
                        merged: false,
                    },
                ],
            })
        );
        assert_eq!(
            git.calls.borrow().as_slice(),
            &[
                vec!["rev-parse", "--is-inside-work-tree"],
                vec!["worktree", "list", "--porcelain"],
                vec!["status", "--porcelain"],
                vec!["status", "--porcelain"],
                vec![
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/usagi/",
                ],
                vec!["merge-base", "--is-ancestor", "usagi/clean", "HEAD"],
                vec!["merge-base", "--is-ancestor", "usagi/detached", "HEAD",],
            ]
        );
    }

    #[test]
    fn repository_observation_fails_closed_when_git_evidence_is_missing() {
        let outside = FakeGit::new(vec![fail("not a repository")]);
        assert_eq!(
            observe_repository(&outside, Path::new("/repo")).unwrap(),
            None
        );

        let worktrees_unavailable = FakeGit::new(vec![ok("true"), fail("broken worktrees")]);
        assert!(observe_repository(&worktrees_unavailable, Path::new("/repo")).is_err());

        let branches_unavailable = FakeGit::new(vec![
            ok("true"),
            ok("worktree /repo\nHEAD root\nbranch refs/heads/main\n"),
            fail("broken refs"),
        ]);
        assert!(
            observe_repository(&branches_unavailable, Path::new("/repo"))
                .unwrap_err()
                .to_string()
                .contains("broken refs")
        );
    }

    /// Only a helper whose build is gone *and* which this data home does not
    /// account for is residue. Either condition alone describes an ordinary
    /// process: a live daemon's executable exists, and a healthy daemon started
    /// from another data home is not ours to end.
    #[test]
    fn only_an_unaccounted_helper_from_a_vanished_build_is_reaped() {
        let inventory = CleanInventory {
            processes: vec![
                // Residue: the worktree that built it was removed, and nothing
                // here refers to it.
                process(
                    31,
                    HelperRole::Broker,
                    "/gone/target/debug/usagi",
                    false,
                    false,
                ),
                process(
                    30,
                    HelperRole::Daemon,
                    "/gone/target/debug/usagi",
                    false,
                    false,
                ),
                // The running daemon of this data home.
                process(40, HelperRole::Daemon, "/usr/local/bin/usagi", true, true),
                // Someone else's daemon: its build exists, so it is current.
                process(41, HelperRole::Daemon, "/opt/usagi/bin/usagi", true, false),
                // Accounted for, even though its build was replaced underneath
                // it — this data home is still relying on it.
                process(
                    42,
                    HelperRole::Broker,
                    "/gone/target/debug/usagi",
                    false,
                    true,
                ),
                // Not spawned the way the daemon and broker are spawned, so its
                // identity cannot be judged from here.
                process(43, HelperRole::Broker, "usagi", false, false),
                // The pid was gone before its start identity could be read, so
                // signalling it later could hit whoever inherits the number.
                ObservedProcess {
                    start_identity: String::new(),
                    ..process(
                        44,
                        HelperRole::Broker,
                        "/gone/target/debug/usagi",
                        false,
                        false,
                    )
                },
            ],
            ..CleanInventory::default()
        };

        assert_eq!(
            plan(&inventory),
            vec![
                // Sorted by pid, and a daemon needs an explicit force because it
                // may still own PTYs; a broker owns nothing.
                CleanCandidate::Process {
                    pid: 30,
                    role: HelperRole::Daemon,
                    executable: "/gone/target/debug/usagi".into(),
                    start_identity: "identity-30".into(),
                    requires_force: true,
                },
                CleanCandidate::Process {
                    pid: 31,
                    role: HelperRole::Broker,
                    executable: "/gone/target/debug/usagi".into(),
                    start_identity: "identity-31".into(),
                    requires_force: false,
                },
            ]
        );
    }

    /// An empty session list is *absence of evidence*, not evidence of absence.
    ///
    /// A lifecycle document that lists no sessions authorises removing every
    /// managed worktree and branch of that workspace at once — including work
    /// that was never pushed. The document is empty in states that have nothing
    /// to do with the worktrees on disk (a workspace the daemon released, a
    /// document written before the sessions were recorded), so it must not be
    /// read as "none of these worktrees is linked".
    #[test]
    fn an_empty_session_list_does_not_authorise_removing_every_worktree() {
        let inventory = CleanInventory {
            processes: Vec::new(),
            registered: Vec::new(),
            daemon_data: vec![DaemonWorkspaceData {
                root: "/a".into(),
                dir: "/data/a".into(),
                root_exists: true,
                sessions: Some(BTreeSet::new()),
            }],
            repositories: vec![RepositoryInventory {
                root: "/a".into(),
                worktrees: vec![ObservedWorktree {
                    path: "/a/.usagi/sessions/live".into(),
                    branch: Some("usagi/live".into()),
                    dirty: false,
                }],
                branches: vec![ObservedBranch {
                    name: "usagi/live".into(),
                    merged: true,
                }],
            }],
        };

        assert_eq!(
            plan(&inventory),
            vec![
                CleanCandidate::Worktree {
                    root: "/a".into(),
                    path: "/a/.usagi/sessions/live".into(),
                    requires_force: true,
                },
                CleanCandidate::Branch {
                    root: "/a".into(),
                    name: "usagi/live".into(),
                    requires_force: true,
                },
            ],
            "an empty session list must not authorise an unforced removal"
        );
    }

    #[test]
    fn plans_only_unlinked_managed_resources_in_stable_order() {
        let inventory = CleanInventory {
            processes: Vec::new(),
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
            processes: Vec::new(),
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
            processes: Vec::new(),
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
            processes: Vec::new(),
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

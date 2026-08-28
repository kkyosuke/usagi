//! Production IO adapter for `usagi clean`.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use fs2::FileExt;
use usagi_core::infrastructure::git::{
    GitOutput, GitRunner, confined_git_command, delete_branch, list_worktrees, remove_worktree,
};
use usagi_core::infrastructure::paths::{self, SESSIONS_DIR, STATE_DIR};
use usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::infrastructure::workspace_state;
use usagi_core::usecase::clean::{
    CleanCandidate, CleanInventory, DaemonWorkspaceData, ObservedBranch, ObservedWorktree,
    RegisteredWorkspace, RepositoryInventory,
};

/// Discover and optionally remove orphan resources. Discovery is always done
/// first, so an apply run prints the exact same plan it is about to execute.
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
pub(crate) fn run(
    out: &mut dyn Write,
    err: &mut dyn Write,
    apply: bool,
    force: bool,
) -> io::Result<ExitCode> {
    let inventory = discover()?;
    let candidates = usagi_core::usecase::clean::plan(&inventory);
    if candidates.is_empty() {
        writeln!(out, "clean: no unlinked resources found")?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(
        out,
        "clean: {} unlinked resource(s){}",
        candidates.len(),
        if apply { "" } else { " (dry-run)" }
    )?;
    for candidate in &candidates {
        writeln!(out, "  {}", describe(candidate))?;
    }
    if !apply {
        writeln!(out, "run `usagi clean --apply` to remove safe candidates")?;
        if candidates.iter().any(CleanCandidate::requires_force) {
            writeln!(
                out,
                "run `usagi clean --apply --force` to also remove protected Git candidates"
            )?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    let storage = Storage::open_default().map_err(io::Error::other)?;
    let mut removed = 0usize;
    let mut skipped = 0usize;
    for candidate in &candidates {
        if candidate.requires_force() && !force {
            writeln!(err, "clean: skipped protected {}", describe(candidate))?;
            skipped += 1;
            continue;
        }
        match apply_candidate(candidate, &storage, force) {
            Ok(()) => removed += 1,
            Err(error) => {
                writeln!(err, "clean: skipped {}: {error}", describe(candidate))?;
                skipped += 1;
            }
        }
    }
    writeln!(out, "clean: removed {removed}, skipped {skipped}")?;
    Ok(if skipped == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn discover() -> io::Result<CleanInventory> {
    let storage = Storage::open_default().map_err(io::Error::other)?;
    let registered = storage
        .load_workspaces()
        .map_err(io::Error::other)?
        .into_iter()
        .map(|workspace| RegisteredWorkspace {
            exists: workspace.path.is_dir(),
            path: workspace.path,
        })
        .collect();
    let daemon_dir = paths::data_dir().map_err(io::Error::other)?.join("daemon");
    let states = workspace_state::adopted(&daemon_dir).map_err(io::Error::other)?;
    let mut daemon_data = Vec::with_capacity(states.len());
    let mut repositories = Vec::new();
    let git = SystemGit;
    for state in states {
        let root_exists = state.root().is_dir();
        let sessions = DaemonLifecycleStore::new(state.dir())
            .load()
            .ok()
            .flatten()
            .map(|lifecycle| {
                lifecycle
                    .sessions
                    .into_iter()
                    .map(|session| session.name)
                    .collect::<BTreeSet<_>>()
            });
        daemon_data.push(DaemonWorkspaceData {
            root: state.root().to_path_buf(),
            dir: state.dir().to_path_buf(),
            root_exists,
            sessions,
        });
        if root_exists && let Some(repository) = discover_repository(&git, state.root())? {
            repositories.push(repository);
        }
    }
    Ok(CleanInventory {
        registered,
        daemon_data,
        repositories,
    })
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn discover_repository(
    git: &dyn GitRunner,
    root: &Path,
) -> io::Result<Option<RepositoryInventory>> {
    let probe = git
        .run(root, &["rev-parse", "--is-inside-work-tree"])
        .map_err(io::Error::other)?;
    if !probe.success || probe.stdout.trim() != "true" {
        return Ok(None);
    }
    let expected_parent = root.join(STATE_DIR).join(SESSIONS_DIR);
    let mut worktrees = Vec::new();
    for worktree in list_worktrees(git, root).map_err(io::Error::other)? {
        if worktree.path.parent() != Some(expected_parent.as_path()) {
            continue;
        }
        let status = git
            .run(&worktree.path, &["status", "--porcelain"])
            .map_err(io::Error::other)?;
        worktrees.push(ObservedWorktree {
            path: worktree.path,
            dirty: !status.success || !status.stdout.trim().is_empty() || worktree.branch.is_none(),
            branch: worktree.branch,
        });
    }
    let refs = git
        .run(
            root,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/usagi/",
            ],
        )
        .map_err(io::Error::other)?;
    if !refs.success {
        return Err(io::Error::other(format!(
            "git branch inventory failed: {}",
            refs.stderr.trim()
        )));
    }
    let mut branches = Vec::new();
    for name in refs
        .stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let merged = git
            .run(root, &["merge-base", "--is-ancestor", name, "HEAD"])
            .map_err(io::Error::other)?
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

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn apply_candidate(candidate: &CleanCandidate, storage: &Storage, force: bool) -> io::Result<()> {
    match candidate {
        CleanCandidate::Workspace { path } => {
            if path.exists() {
                return Err(io::Error::other("workspace path exists again"));
            }
            usagi_core::usecase::workspace::remove(storage, std::slice::from_ref(path))
                .map_err(io::Error::other)?;
            Ok(())
        }
        CleanCandidate::Data { root, dir } => {
            if root.exists() {
                return Err(io::Error::other("workspace root exists again"));
            }
            let _daemon_fence = acquire_daemon_fence()?;
            remove_daemon_data(dir)
        }
        CleanCandidate::Worktree { root, path, .. } => {
            let _fence = acquire_workspace_fence(root)?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| io::Error::other("worktree has no canonical session name"))?;
            ensure_unlinked(root, name)?;
            remove_worktree(&SystemGit, root, path, force).map_err(io::Error::other)
        }
        CleanCandidate::Branch { root, name, .. } => {
            let _fence = acquire_workspace_fence(root)?;
            let session = name
                .strip_prefix("usagi/")
                .ok_or_else(|| io::Error::other("branch is outside the usagi namespace"))?;
            ensure_unlinked(root, session)?;
            delete_branch(&SystemGit, root, name, force).map_err(io::Error::other)
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn remove_daemon_data(dir: &Path) -> io::Result<()> {
    let container = paths::data_dir()
        .map_err(io::Error::other)?
        .join("daemon")
        .join(paths::WORKSPACE_STATE_DIR);
    if dir.parent() != Some(container.as_path()) {
        return Err(io::Error::other(
            "daemon data target is outside the workspace-state container",
        ));
    }
    let metadata = std::fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "daemon data target is not a real directory",
        ));
    }
    std::fs::remove_dir_all(dir)
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn ensure_unlinked(root: &Path, name: &str) -> io::Result<()> {
    let daemon_dir = paths::data_dir().map_err(io::Error::other)?.join("daemon");
    let state = workspace_state::adopted(&daemon_dir)
        .map_err(io::Error::other)?
        .into_iter()
        .find(|state| state.root() == root)
        .ok_or_else(|| io::Error::other("workspace lifecycle state is unavailable"))?;
    let lifecycle = DaemonLifecycleStore::new(state.dir())
        .load()
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("workspace lifecycle document is unavailable"))?;
    if lifecycle
        .sessions
        .iter()
        .any(|session| session.name == name)
    {
        return Err(io::Error::other("resource became linked to a live session"));
    }
    Ok(())
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn acquire_workspace_fence(root: &Path) -> io::Result<File> {
    let path = paths::workspace_fence_path(root);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("workspace fence has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock_exclusive().map_err(|_| {
        io::Error::other("workspace is owned by a running daemon; stop it and retry")
    })?;
    Ok(file)
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
fn acquire_daemon_fence() -> io::Result<File> {
    let path = paths::data_dir()
        .map_err(io::Error::other)?
        .join("daemon")
        .join("daemon.lock");
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("daemon fence has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock_exclusive()
        .map_err(|_| io::Error::other("daemon is running; stop it and retry data cleanup"))?;
    Ok(file)
}

fn describe(candidate: &CleanCandidate) -> String {
    match candidate {
        CleanCandidate::Workspace { path } => format!("workspace {}", path.display()),
        CleanCandidate::Data { root, dir } => {
            format!(
                "data {} (missing workspace {})",
                dir.display(),
                root.display()
            )
        }
        CleanCandidate::Worktree {
            path,
            requires_force,
            ..
        } => format!(
            "worktree {}{}",
            path.display(),
            if *requires_force {
                " [force required]"
            } else {
                ""
            }
        ),
        CleanCandidate::Branch {
            name,
            root,
            requires_force,
        } => format!(
            "branch {name} in {}{}",
            root.display(),
            if *requires_force {
                " [force required]"
            } else {
                ""
            }
        ),
    }
}

struct SystemGit;

impl GitRunner for SystemGit {
    #[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=clean_planner_classifies_all_effects
    fn run(&self, repo: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        let output = confined_git_command(repo).args(args).output()?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::describe;
    use usagi_core::usecase::clean::CleanCandidate;

    #[test]
    fn clean_planner_classifies_all_effects() {
        let rendered = [
            CleanCandidate::Workspace {
                path: "/missing".into(),
            },
            CleanCandidate::Data {
                root: "/missing".into(),
                dir: "/data/w/one".into(),
            },
            CleanCandidate::Worktree {
                root: "/repo".into(),
                path: "/repo/.usagi/sessions/x".into(),
                requires_force: true,
            },
            CleanCandidate::Branch {
                root: "/repo".into(),
                name: "usagi/x".into(),
                requires_force: true,
            },
        ]
        .map(|candidate| describe(&candidate));
        assert!(rendered[0].starts_with("workspace "));
        assert!(rendered[1].contains("missing workspace"));
        assert!(rendered[2].contains("force required"));
        assert!(rendered[3].contains("usagi/x"));
    }
}

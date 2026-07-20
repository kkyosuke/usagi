//! Reconcile the on-disk session tree with the sessions recorded in
//! `state.json`, quarantining strays left by interrupted creates, crashes, or
//! hand-edited state until their ownership can be reviewed explicitly.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::tree;
use crate::domain::workspace_state::{
    PendingSessionRemoval, SessionRemovalPhase, WorkspaceState, WorktreeProvenance,
};
use crate::infrastructure::git;
use crate::infrastructure::repo_paths::{SESSIONS_DIR, STATE_DIR};
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Reconcile the on-disk session tree under `.usagi/sessions/` with the sessions
/// recorded in `state.json`. Every *directory* there that has no matching record
/// is a stray — left by an interrupted create, a hand-edited `state.json`, or a
/// crash — and is durably quarantined as an orphaned pending removal. Reconcile
/// never force-deletes it because the missing record means ownership cannot be
/// established safely. Loose files are left untouched.
///
/// Returns the stray directories newly quarantined by this pass.
///
/// This is the public, self-locking entry point: it acquires the workspace
/// store lock for the duration of the scan-and-quarantine so it never races a
/// concurrent writer. [`create`](super::create) and [`remove`](super::remove)
/// already hold that lock across their whole operation and call
/// [`reconcile_locked`] directly instead, so the load-and-destroy here cannot
/// delete a worktree another process has built but not yet recorded.
pub fn reconcile(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let store = WorkspaceStore::new(workspace_root);
    let _lock = store.lock()?;
    reconcile_locked(workspace_root)
}

/// Reconcile assuming the caller already holds the workspace store lock (see
/// [`WorkspaceStore::lock`]). [`create`](super::create) and
/// [`remove`](super::remove) hold the lock across reconcile → build/teardown →
/// record so the whole sequence is serialised against other usagi processes;
/// they call this directly to avoid re-acquiring the non-reentrant lock.
pub(super) fn reconcile_locked(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let sessions_base = workspace_root.join(STATE_DIR).join(SESSIONS_DIR);
    if !sessions_base.is_dir() {
        return Ok(Vec::new());
    }

    let store = WorkspaceStore::new(workspace_root);
    let mut state = store.load()?.unwrap_or_else(WorkspaceState::new);
    let recorded: HashSet<String> = state
        .sessions
        .iter()
        .map(|session| session.name.clone())
        .chain(
            state
                .pending_removals
                .iter()
                .map(|pending| pending.name.clone()),
        )
        .collect();

    // Cheap pre-check before the expensive rescan: match the recorded session
    // names against the directory names directly under `.usagi/sessions/`. When
    // every on-disk session directory is recorded there are no strays, so skip
    // the full `source_repos` walk and the per-repository `list_worktrees`
    // entirely — the common case on every `create`/`remove`.
    let strays: Vec<(PathBuf, String)> = fs::read_dir(&sessions_base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            (
                entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
            )
        })
        .filter(|(_, name)| !recorded.contains(name))
        .collect();
    if strays.is_empty() {
        return Ok(Vec::new());
    }

    let mut quarantined = Vec::new();
    for (stray, name) in strays {
        state.pending_removals.push(PendingSessionRemoval {
            name,
            root: stray.clone(),
            worktrees: Vec::new(),
            provenance: Vec::new(),
            phase: SessionRemovalPhase::Orphaned,
        });
        quarantined.push(stray);
    }
    state.updated_at = chrono::Utc::now();
    store.save(&state)?;
    Ok(quarantined)
}

/// Each source repository under `workspace_root` paired with its worktrees,
/// listed once. Sharing a single listing across every stray is sound because
/// destroying a session only removes the worktree on that session's unique
/// branch (the directory name), so the listing stays valid across sessions.
pub(super) fn list_repo_worktrees(
    workspace_root: &Path,
) -> Result<Vec<(PathBuf, Vec<git::WorktreeInfo>)>> {
    tree::source_repos(workspace_root)
        .into_iter()
        .map(|repo| {
            let worktrees = git::list_worktrees(&repo)?;
            Ok((repo, worktrees))
        })
        .collect()
}

/// Physically destroy one session whose directory is `root` and whose branch is
/// `branch`: preflight every candidate against recorded repository/worktree
/// provenance, canonical containment, and the branch before issuing any effect.
/// With `force`, a dirty worktree may be discarded. Infrastructure failures
/// (including locked worktrees) still abort before the directory is deleted so
/// the durable caller can retain context and retry. Already-absent components
/// remain successful, making partial teardown idempotent.
///
/// Used by [`remove`](super::remove); reconcile quarantines unowned strays and
/// therefore never calls this destructive primitive.
/// `repo_worktrees` is each source repository paired with its worktrees, from
/// [`list_repo_worktrees`].
#[derive(Debug)]
pub(super) struct OwnershipError(String);

impl std::fmt::Display for OwnershipError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for OwnershipError {}

fn ownership_error(message: impl Into<String>) -> anyhow::Error {
    OwnershipError(message.into()).into()
}

fn canonical_git_common_dir(path: &Path) -> Result<PathBuf> {
    let common = git::git_common_dir(path).ok_or_else(|| {
        ownership_error(format!(
            "cannot resolve Git repository identity for {}",
            path.display()
        ))
    })?;
    fs::canonicalize(&common).map_err(|error| {
        ownership_error(format!(
            "cannot canonicalize Git repository identity {}: {error}",
            common.display()
        ))
    })
}

pub(super) fn discard_session(
    root: &Path,
    branch: &str,
    provenance: &[WorktreeProvenance],
    repo_worktrees: &[(PathBuf, Vec<git::WorktreeInfo>)],
    force: bool,
) -> Result<()> {
    if provenance.is_empty() {
        return Err(ownership_error(format!(
            "session {} has no recorded worktree provenance; clean it up manually",
            root.display()
        )));
    }
    let root_metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut recorded_repos = HashSet::new();
            let mut recorded_worktrees = HashSet::new();
            for recorded in provenance {
                let repo = fs::canonicalize(&recorded.repo).map_err(|repo_error| {
                    ownership_error(format!(
                        "cannot canonicalize recorded repository {}: {repo_error}",
                        recorded.repo.display()
                    ))
                })?;
                if !recorded_repos.insert(repo.clone())
                    || !recorded_worktrees.insert(recorded.worktree.clone())
                {
                    return Err(ownership_error("duplicate recorded worktree provenance"));
                }
                let recorded_common = canonical_git_common_dir(&recorded.repo)?;
                let expected_repo_present = repo_worktrees.iter().any(|(candidate, _)| {
                    fs::canonicalize(candidate).is_ok_and(|candidate| candidate == repo)
                        && canonical_git_common_dir(candidate)
                            .is_ok_and(|candidate_common| candidate_common == recorded_common)
                });
                if !expected_repo_present {
                    return Err(ownership_error(format!(
                        "recorded repository {} is not in the expected repository set",
                        recorded.repo.display()
                    )));
                }
            }
            let registered = repo_worktrees.iter().any(|(_, worktrees)| {
                worktrees.iter().any(|worktree| {
                    worktree.branch.as_deref() == Some(branch)
                        || provenance
                            .iter()
                            .any(|recorded| recorded.worktree == worktree.path)
                })
            });
            let branch_remains = repo_worktrees
                .iter()
                .any(|(repo, _)| git::branch_exists(repo, branch));
            if registered || branch_remains {
                return Err(ownership_error(format!(
                    "cannot prove session root {}: {error}",
                    root.display()
                )));
            }
            // A prior teardown attempt already removed every recorded target and
            // branch. There is no remaining effect to authorize, so the retry is
            // an idempotent success even though the old path no longer resolves.
            return Ok(());
        }
        Err(error) => {
            return Err(ownership_error(format!(
                "cannot prove session root {}: {error}",
                root.display()
            )));
        }
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ownership_error(format!(
            "session root {} is not an unambiguous directory",
            root.display()
        )));
    }
    let root_canon = fs::canonicalize(root).map_err(|error| {
        ownership_error(format!(
            "cannot canonicalize session root {}: {error}",
            root.display()
        ))
    })?;

    let mut expected = Vec::new();
    for recorded in provenance {
        let repo = fs::canonicalize(&recorded.repo).map_err(|error| {
            ownership_error(format!(
                "cannot canonicalize recorded repository {}: {error}",
                recorded.repo.display()
            ))
        })?;
        let repo_common = canonical_git_common_dir(&recorded.repo)?;
        let worktree = match fs::symlink_metadata(&recorded.worktree) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(ownership_error(format!(
                        "recorded worktree {} is not an unambiguous directory",
                        recorded.worktree.display()
                    )));
                }
                let worktree = fs::canonicalize(&recorded.worktree).map_err(|error| {
                    ownership_error(format!(
                        "cannot canonicalize recorded worktree {}: {error}",
                        recorded.worktree.display()
                    ))
                })?;
                if !worktree.starts_with(&root_canon) {
                    return Err(ownership_error(format!(
                        "recorded worktree {} escapes session root {}",
                        worktree.display(),
                        root_canon.display()
                    )));
                }
                Some(worktree)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(ownership_error(format!(
                    "cannot inspect recorded worktree {}: {error}",
                    recorded.worktree.display()
                )));
            }
        };
        if expected.iter().any(
            |(known_repo, known_common, known_recorded, known_worktree): &(
                PathBuf,
                PathBuf,
                PathBuf,
                Option<PathBuf>,
            )| {
                *known_repo == repo
                    || *known_common == repo_common
                    || *known_recorded == recorded.worktree
                    || known_worktree
                        .as_ref()
                        .is_some_and(|known| worktree.as_ref() == Some(known))
            },
        ) {
            return Err(ownership_error("duplicate recorded worktree provenance"));
        }
        expected.push((repo, repo_common, recorded.worktree.clone(), worktree));
    }

    let mut targets = Vec::new();
    for (repo, worktrees) in repo_worktrees {
        let repo_canon = fs::canonicalize(repo).map_err(|error| {
            ownership_error(format!(
                "cannot canonicalize expected repository {}: {error}",
                repo.display()
            ))
        })?;
        let repo_common = canonical_git_common_dir(repo)?;
        for wt in worktrees {
            let branch_matches = wt.branch.as_deref() == Some(branch);
            let path_canon = fs::canonicalize(&wt.path);
            let candidate_common =
                git::git_common_dir(&wt.path).and_then(|common| fs::canonicalize(common).ok());
            let identity = expected.iter().find(
                |(expected_repo, expected_common, recorded_worktree, expected_worktree)| {
                    *expected_repo == repo_canon
                        && candidate_common.as_ref() == Some(expected_common)
                        && *expected_common == repo_common
                        && (recorded_worktree == &wt.path
                            || path_canon
                                .as_ref()
                                .ok()
                                .is_some_and(|path| expected_worktree.as_ref() == Some(path)))
                },
            );
            if branch_matches || identity.is_some() {
                let path = path_canon.map_err(|error| {
                    ownership_error(format!(
                        "cannot canonicalize candidate worktree {}: {error}",
                        wt.path.display()
                    ))
                })?;
                if !branch_matches || identity.is_none() || !path.starts_with(&root_canon) {
                    return Err(ownership_error(format!(
                        "worktree {} lacks complete ownership proof",
                        wt.path.display()
                    )));
                }
                targets.push((repo.clone(), wt.path.clone(), path));
            }
        }
    }

    for (_, _, _, worktree) in &expected {
        if worktree
            .as_ref()
            .is_some_and(|worktree| !targets.iter().any(|(_, _, target)| target == worktree))
        {
            return Err(ownership_error(format!(
                "recorded worktree {} is not registered in its expected repository",
                worktree.as_ref().unwrap().display()
            )));
        }
    }

    for (repo, worktree, _) in &targets {
        git::remove_worktree(repo, worktree, force)?;
    }

    // Delete the session tree *before* pruning and dropping the branch below.
    // This ordering is what keeps the name reusable: a worktree whose directory
    // vanished out-of-band (a crash, a manual `rm`, an external cleanup) — or one
    // a forced/locked `worktree remove` above failed to unregister — leaves a
    // registration that still holds the session branch checked out, which makes
    // `git branch -D` refuse. Removing the directory first turns that into a
    // prunable registration, so the prune clears it and the branch is no longer
    // checked out anywhere; only then can it actually be deleted.
    if root.exists() {
        fs::remove_dir_all(root).context(format!("failed to remove {}", root.display()))?;
    }

    for (repo, _) in repo_worktrees {
        git::prune_worktrees(repo)?;
        if git::branch_exists(repo, branch) {
            git::delete_branch(repo, branch)?;
        }
    }
    Ok(())
}

//! Reconcile the on-disk session tree with the sessions recorded in
//! `state.json`, quarantining strays left by interrupted creates, crashes, or
//! hand-edited state until their ownership can be reviewed explicitly.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::tree;
use crate::domain::agent::Agent;
use crate::domain::workspace_state::{
    PendingSessionRemoval, QuarantineOrigin, SessionRemovalPhase, WorkspaceState,
    WorktreeProvenance,
};
use crate::infrastructure::git;
use crate::infrastructure::repo_paths::{SESSIONS_DIR, STATE_DIR, TRASH_DIR};
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Reconcile the on-disk session tree under `.usagi/sessions/` with the sessions
/// recorded in `state.json`. Every *directory* there that has no matching record
/// is a stray — left by an interrupted create, a hand-edited `state.json`, or a
/// crash — and is durably quarantined as an orphaned pending removal. Reconcile
/// never force-deletes it because the missing record means ownership cannot be
/// established safely. Loose files are left untouched.
///
/// Returns the stray directories newly quarantined by this pass, together with
/// every interrupted removal it resumed and every retired tree it reclaimed.
///
/// This is the public, self-locking entry point. It acquires the workspace store
/// lock for the duration of the scan-and-quarantine so it never races a
/// concurrent writer, then **releases it** and finishes any removal a crash left
/// mid-transaction ([`resume_pending_removals`](super::resume_pending_removals)).
/// Resuming runs outside the lock because it deletes worktrees and session trees,
/// which can take minutes — exactly the work [`remove`](super::remove) keeps out
/// of the locked window.
///
/// [`create`](super::create) and [`remove`](super::remove) hold the store lock
/// across their own durable transitions and call [`reconcile_locked`] directly
/// instead, so the load-and-quarantine here cannot mistake a worktree another
/// process has built but not yet recorded for a stray.
///
/// Finally it reclaims the trash: the session trees earlier removals renamed
/// aside instead of deleting inline ([`sweep_trash`]). This is the maintenance
/// entry point, so it is where the deletion those removals deferred is actually
/// paid for.
pub fn reconcile(workspace_root: &Path, agent: &dyn Agent) -> Result<ReconcileOutcome> {
    let store = WorkspaceStore::new(workspace_root);
    let quarantined = {
        let _lock = store.lock()?;
        reconcile_locked(workspace_root)?
    };
    Ok(ReconcileOutcome {
        quarantined,
        resumed: super::resume_pending_removals(workspace_root, agent),
        reclaimed: sweep_trash(workspace_root),
    })
}

/// What one [`reconcile`] pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Stray session directories newly quarantined as orphaned pending removals.
    pub quarantined: Vec<PathBuf>,
    /// Interrupted removals this pass attempted to finish.
    pub resumed: Vec<super::ResumedRemoval>,
    /// Retired session trees this pass tried to delete from the trash.
    pub reclaimed: Vec<ReclaimedTree>,
}

/// Reconcile assuming the caller already holds the workspace store lock (see
/// [`WorkspaceStore::lock`]). [`create`](super::create) holds the lock across
/// reconcile → build → record, and [`remove`](super::remove) holds it across
/// reconcile → tombstone, so each sequence is serialised against other usagi
/// processes; they call this directly to avoid re-acquiring the non-reentrant
/// lock.
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
            // A stray has no session record, so there is no recorded branch to
            // copy and no evidence for one: the directory name is all reconcile
            // knows. Recording `None` keeps that honest rather than asserting a
            // branch derived from the name.
            branch: None,
            quarantine: Some(QuarantineOrigin::Reconcile),
            name,
            root: stray.clone(),
            worktrees: Vec::new(),
            provenance: Vec::new(),
            force: false,
            phase: SessionRemovalPhase::Orphaned,
        });
        quarantined.push(stray);
    }
    state.updated_at = chrono::Utc::now();
    store.save(&state)?;
    Ok(quarantined)
}

/// Where `workspace_root`'s retired session trees wait to be deleted:
/// `<workspace>/.usagi/trash/`. See [`TRASH_DIR`] for why teardown renames a
/// session tree here instead of deleting it inline.
pub fn trash_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(STATE_DIR).join(TRASH_DIR)
}

/// One retired session tree a [`sweep_trash`] pass tried to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclaimedTree {
    /// The entry under `.usagi/trash/`.
    pub path: PathBuf,
    /// `None` when the entry is gone. Otherwise why it survived this pass; the
    /// next sweep retries it, and nothing else depends on it being gone.
    pub error: Option<String>,
}

/// Delete every tree retired under `<workspace>/.usagi/trash/`, reclaiming the
/// disk that [`discard_session`] deliberately did not wait for.
///
/// This is the slow half of a removal — deleting a session's `target/` can take
/// minutes — so it deliberately runs *outside* [`remove`](super::remove), which
/// returns as soon as the tree is renamed aside. Callers pick when to pay:
/// [`reconcile`] and `usagi clean` run it synchronously as maintenance, and the
/// long-lived surfaces (the TUI's removal worker, the MCP server) hand it to a
/// background thread ([`spawn_trash_sweep`](super::spawn_trash_sweep)).
///
/// Never fails the caller: reclaiming is pure housekeeping with no tombstone
/// behind it, so a stubborn entry is reported and left for the next pass rather
/// than turned into an error the caller has to handle. It is fail-closed about
/// *what* it deletes, though — an entry is removed only once it canonically
/// resolves to a direct child of the canonical trash directory, so a symlink
/// planted there unlinks the link and never reaches its target.
pub fn sweep_trash(workspace_root: &Path) -> Vec<ReclaimedTree> {
    let trash = trash_dir(workspace_root);
    // No trash directory (the common case) means nothing was ever retired here.
    let Ok(canonical_trash) = fs::canonicalize(&trash) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&trash) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            ReclaimedTree {
                error: reclaim_entry(&canonical_trash, &path)
                    .err()
                    .map(|error| format!("{error:#}")),
                path,
            }
        })
        .collect()
}

/// Delete one entry of the trash directory, refusing anything that does not
/// prove to be a direct child of it.
fn reclaim_entry(canonical_trash: &Path, path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        // A symlink is unlinked, never followed: whatever it points at is not
        // this directory's to delete. A loose file is simply removed.
        return fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()));
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("cannot canonicalize {}", path.display()))?;
    if canonical.parent() != Some(canonical_trash) {
        bail!(
            "refusing to reclaim {}: it resolves to {}, which is not directly inside {}",
            path.display(),
            canonical.display(),
            canonical_trash.display()
        );
    }
    fs::remove_dir_all(&canonical).with_context(|| format!("cannot remove {}", canonical.display()))
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

/// Ownership could not be **proven**: the recorded evidence and the live
/// repositories genuinely disagree. The caller quarantines on this
/// ([`SessionRemovalPhase::Orphaned`]) because retrying cannot change the
/// answer — only an operator can.
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

/// The ownership check could not be **completed**: a filesystem probe it needs
/// failed for a reason unrelated to ownership (a permission error, an unreadable
/// mount, an I/O fault). Distinct from [`OwnershipError`] because the evidence is
/// not in conflict — it is merely unreadable right now — so the caller keeps the
/// removal retryable instead of quarantining a session over a transient fault.
#[derive(Debug)]
pub(super) struct ProbeError(String);

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProbeError {}

fn probe_error(message: impl Into<String>) -> anyhow::Error {
    ProbeError(message.into()).into()
}

fn canonical_git_common_dir(path: &Path) -> Result<PathBuf> {
    // Not resolving to a Git repository at all is an ownership answer (the
    // recorded repository is gone, or was never one); failing to canonicalize a
    // path git *did* report is a probe fault.
    let common = git::git_common_dir(path).ok_or_else(|| {
        ownership_error(format!(
            "cannot resolve Git repository identity for {}",
            path.display()
        ))
    })?;
    fs::canonicalize(&common).map_err(|error| {
        probe_error(format!(
            "cannot canonicalize Git repository identity {}: {error}",
            common.display()
        ))
    })
}

/// Prove that the session rooted at `root` is **still intact**: every worktree it
/// recorded owning exists on disk, is an unambiguous directory canonically inside
/// `root`, and is registered in the repository the record names — matched by
/// canonical repository path and Git common dir, never by branch label.
///
/// This is the read-only counterpart of [`discard_session`]'s preflight and issues
/// no effect whatsoever, so a caller may run it to *decide* something without
/// risking a partial teardown. It deliberately does not share that preflight's
/// tolerance for an already-absent worktree: `discard_session` treats a missing
/// target as an idempotent partial teardown, while this proof answers "may this
/// session be returned to normal?" — and a half-torn-down session may not be. An
/// absent recorded worktree is therefore an ownership failure here, which points
/// the operator at resuming the teardown instead.
///
/// Used by [`release_quarantine`](super::release_quarantine) to check that
/// withdrawing a quarantine leaves a session usagi can go on managing.
pub(super) fn prove_live_session(
    root: &Path,
    provenance: &[WorktreeProvenance],
    repo_worktrees: &[(PathBuf, Vec<git::WorktreeInfo>)],
) -> Result<()> {
    if provenance.is_empty() {
        return Err(ownership_error(format!(
            "session {} has no recorded worktree provenance to check",
            root.display()
        )));
    }
    let root_metadata = fs::symlink_metadata(root).map_err(|error| {
        // A session that is supposed to still be live must have its directory.
        // Absent is an ownership answer; unreadable is a probe fault.
        if error.kind() == std::io::ErrorKind::NotFound {
            ownership_error(format!("session root {} no longer exists", root.display()))
        } else {
            probe_error(format!(
                "cannot inspect session root {}: {error}",
                root.display()
            ))
        }
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(ownership_error(format!(
            "session root {} is not an unambiguous directory",
            root.display()
        )));
    }
    let root_canon = fs::canonicalize(root).map_err(|error| {
        probe_error(format!(
            "cannot canonicalize session root {}: {error}",
            root.display()
        ))
    })?;

    let mut seen: Vec<(PathBuf, PathBuf)> = Vec::new();
    for recorded in provenance {
        let repo = fs::canonicalize(&recorded.repo).map_err(|error| {
            probe_error(format!(
                "cannot canonicalize recorded repository {}: {error}",
                recorded.repo.display()
            ))
        })?;
        let repo_common = canonical_git_common_dir(&recorded.repo)?;
        let metadata = fs::symlink_metadata(&recorded.worktree).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ownership_error(format!(
                    "recorded worktree {} no longer exists",
                    recorded.worktree.display()
                ))
            } else {
                probe_error(format!(
                    "cannot inspect recorded worktree {}: {error}",
                    recorded.worktree.display()
                ))
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ownership_error(format!(
                "recorded worktree {} is not an unambiguous directory",
                recorded.worktree.display()
            )));
        }
        let worktree = fs::canonicalize(&recorded.worktree).map_err(|error| {
            probe_error(format!(
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
        if seen
            .iter()
            .any(|(known_repo, known_worktree)| *known_repo == repo || *known_worktree == worktree)
        {
            return Err(ownership_error("duplicate recorded worktree provenance"));
        }
        // A candidate repository usagi cannot read is simply not a match; the
        // failure surfaces below as "not registered" rather than as a probe fault,
        // because an unreadable *other* repository says nothing about this one.
        let registered = repo_worktrees.iter().any(|(candidate, worktrees)| {
            fs::canonicalize(candidate).is_ok_and(|candidate_canon| candidate_canon == repo)
                && canonical_git_common_dir(candidate).is_ok_and(|common| common == repo_common)
                && worktrees
                    .iter()
                    .any(|wt| fs::canonicalize(&wt.path).is_ok_and(|path| path == worktree))
        });
        if !registered {
            return Err(ownership_error(format!(
                "recorded worktree {} is not registered in its recorded repository {}",
                recorded.worktree.display(),
                recorded.repo.display()
            )));
        }
        seen.push((repo, worktree));
    }
    Ok(())
}

/// What one [`discard_session`] left behind that the caller should mention.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DiscardOutcome {
    /// Branches that were checked out in the worktrees this teardown removed but
    /// are **not** the session's recorded branch — a branch the session cut or
    /// renamed after creation (`git switch -c`, `git branch -m`).
    ///
    /// Teardown deletes only the recorded branch, so these still exist
    /// afterwards. They are reported rather than deleted: usagi has no record
    /// that it created them, and silently dropping a branch holding a session's
    /// work is exactly the destructive guess the ownership rules exist to
    /// prevent. Sorted and deduplicated.
    pub retained_branches: Vec<String>,
}

/// Physically destroy one session whose directory is `root` and whose recorded
/// branch is `branch`: preflight every candidate against recorded
/// repository/worktree provenance and canonical containment in `root` before
/// issuing any effect. `branch` selects what may be *deleted* (and candidates
/// carrying it are still matched, so an impostor on the same branch is caught) —
/// it is never what *proves* ownership, because a session's worktree may
/// legitimately have moved to another branch since creation.
/// With `force`, a dirty worktree may be discarded. Infrastructure failures
/// (including locked worktrees) still abort before the directory is touched so
/// the durable caller can retain context and retry. Already-absent components
/// remain successful, making partial teardown idempotent.
///
/// The session tree is **retired, not deleted**: once every candidate has passed
/// both the ownership proof and git's own removal refusals
/// ([`git::ensure_worktree_removable`]), the whole tree is renamed into `trash`
/// in one move and the now-dangling worktree registrations are cleared by the
/// prune below. That is what makes teardown independent of how big the session
/// grew — deleting a `target/` of several gigabytes, whether through
/// `git worktree remove` or `remove_dir_all`, is the minutes-long part, and it
/// now happens later and off the caller's path ([`sweep_trash`]). A rename that
/// cannot be done (a trash directory on another filesystem, say) falls back to
/// deleting the tree inline, which is slower but equally correct.
///
/// Used by [`remove`](super::remove); reconcile quarantines unowned strays and
/// therefore never calls this destructive primitive.
/// `repo_worktrees` is each source repository paired with its worktrees, from
/// [`list_repo_worktrees`]; `trash` is [`trash_dir`] for the workspace this
/// session belongs to.
pub(super) fn discard_session(
    root: &Path,
    branch: &str,
    provenance: &[WorktreeProvenance],
    repo_worktrees: &[(PathBuf, Vec<git::WorktreeInfo>)],
    force: bool,
    trash: &Path,
) -> Result<DiscardOutcome> {
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
                    probe_error(format!(
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
            return Ok(DiscardOutcome::default());
        }
        Err(error) => {
            // Not "the root is gone" (handled above) but "the root cannot be
            // read": a probe fault, so the removal stays retryable.
            return Err(probe_error(format!(
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
        probe_error(format!(
            "cannot canonicalize session root {}: {error}",
            root.display()
        ))
    })?;

    let mut expected = Vec::new();
    for recorded in provenance {
        let repo = fs::canonicalize(&recorded.repo).map_err(|error| {
            probe_error(format!(
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
                    probe_error(format!(
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
                return Err(probe_error(format!(
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
    let mut retained_branches = Vec::new();
    for (repo, worktrees) in repo_worktrees {
        let repo_canon = fs::canonicalize(repo).map_err(|error| {
            probe_error(format!(
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
                    probe_error(format!(
                        "cannot canonicalize candidate worktree {}: {error}",
                        wt.path.display()
                    ))
                })?;
                // Ownership rests on *identity* — recorded repository, Git
                // common dir, recorded worktree path, and canonical containment
                // in this session root — never on the branch label. A candidate
                // that matches the branch but no recorded identity is a
                // different worktree that happens to share the name, and stays
                // fail-closed. The reverse (identity proven, branch moved on) is
                // an ordinary `git switch -c` inside the session and must not
                // make the session undeletable: the label a worktree carries is
                // not evidence of who owns it.
                if identity.is_none() || !path.starts_with(&root_canon) {
                    return Err(ownership_error(format!(
                        "worktree {} lacks complete ownership proof",
                        wt.path.display()
                    )));
                }
                if let Some(checked_out) = wt.branch.as_deref().filter(|_| !branch_matches) {
                    retained_branches.push(checked_out.to_string());
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

    // Ask git what it would refuse *before* anything moves. Retiring the tree is
    // a single rename over every worktree at once, so there is no per-worktree
    // point left at which git could decline: an unforced teardown that would have
    // been stopped by `git worktree remove` on a dirty worktree must be stopped
    // here instead, with the session still intact and the removal retryable.
    for (repo, worktree, _) in &targets {
        git::ensure_worktree_removable(repo, worktree, force)?;
    }

    // Retire the session tree *before* pruning and dropping the branch below.
    // This ordering is what keeps the name reusable: a worktree whose directory
    // vanished out-of-band (a crash, a manual `rm`, an external cleanup) leaves a
    // registration that still holds the session branch checked out, which makes
    // `git branch -D` refuse. Moving the directory away turns every one of them
    // into a prunable registration, so the prune clears them and the branch is no
    // longer checked out anywhere; only then can it actually be deleted.
    if root.exists() {
        retire_session_tree(root, trash)?;
    }

    // Only the *recorded* branch is deleted. A branch the session moved onto
    // afterwards is not usagi's to drop — see [`DiscardOutcome::retained_branches`].
    for (repo, _) in repo_worktrees {
        git::prune_worktrees(repo)?;
        if git::branch_exists(repo, branch) {
            git::delete_branch(repo, branch)?;
        }
    }
    retained_branches.sort();
    retained_branches.dedup();
    Ok(DiscardOutcome { retained_branches })
}

/// How a session tree left the sessions directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Retirement {
    /// Renamed into the trash directory, to be deleted by a later
    /// [`sweep_trash`]. The O(1) path taken whenever the rename is possible.
    Retired(PathBuf),
    /// Deleted in place because the rename was not possible — the trash sits on
    /// another filesystem (`EXDEV`), or could not be created at all. Correct,
    /// but as slow as the tree is big.
    Deleted,
}

/// Move `root` out of the sessions directory, falling back to deleting it.
///
/// Either way the session path is free the moment this returns, which is what
/// the teardown ordering (and reusing the name) depends on.
fn retire_session_tree(root: &Path, trash: &Path) -> Result<Retirement> {
    // Wrapped rather than passed as `fs::rename` directly: the generic function
    // item only satisfies the higher-ranked bound through a closure.
    retire_session_tree_with(root, trash, |from, to| fs::rename(from, to))
}

/// [`retire_session_tree`] with the rename injected, so both branches — the
/// O(1) one and the fallback taken when the filesystem cannot rename — are
/// exercised on every platform the tests run on.
pub(super) fn retire_session_tree_with(
    root: &Path,
    trash: &Path,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<Retirement> {
    if let Some(name) = root.file_name().and_then(|name| name.to_str()) {
        let destination = trash.join(retired_component(name, &removal_id()));
        if fs::create_dir_all(trash).is_ok() && rename(root, &destination).is_ok() {
            return Ok(Retirement::Retired(destination));
        }
    }
    fs::remove_dir_all(root).context(format!("failed to remove {}", root.display()))?;
    Ok(Retirement::Deleted)
}

/// Longest prefix of a session name kept in its trash directory's name. A
/// session name may be up to 250 bytes (`MAX_SESSION_NAME_BYTES`), which leaves
/// no room for the removal id inside the portable 255-byte `NAME_MAX`, so the
/// readable part is capped and the id — not the name — is what makes the
/// component unique.
const RETIRED_NAME_PREFIX_BYTES: usize = 64;

/// The directory component a session tree named `name` is retired to, labelled
/// so an operator looking in `.usagi/trash/` can tell what a leftover was.
pub(super) fn retired_component(name: &str, id: &str) -> String {
    let mut end = name.len().min(RETIRED_NAME_PREFIX_BYTES);
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}-{id}", &name[..end])
}

/// An identifier unique to one retirement: the wall clock pins it in time for a
/// human reading the directory listing, while the process id and the counter
/// keep two removals in the same millisecond — or in two usagi processes — from
/// choosing the same name.
fn removal_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{}-{}-{sequence}",
        chrono::Utc::now().format("%Y%m%d%H%M%S%3f"),
        std::process::id()
    )
}

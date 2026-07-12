#![coverage(off)]

//! Session lifecycle operations.
//!
//! Two layers live here. The **state primitives** — [`list`], [`get`], [`touch`],
//! [`record`], [`remove_record`] — read-modify-write the persisted
//! [`WorkspaceState`](crate::domain::workspace_state::WorkspaceState) in
//! `state.json` through the injected [`WorkspaceStateStore`], holding its
//! cross-process lock across each mutation. The **full lifecycle** — [`create`]
//! and [`remove`] — additionally builds and tears down the session's git worktree
//! through the injected [`GitRunner`], composing the git and state layers with a
//! defined order and rollback (see each function).
//!
//! The clock is passed in (`now`) so these stay clock-free and fully testable.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::note::Scratchpad;
use crate::domain::session::{SessionOrigin, SessionRecord};
use crate::infrastructure::git::{GitRunner, add_worktree, remove_worktree};
use crate::infrastructure::paths::{SESSIONS_DIR, STATE_DIR};
use crate::infrastructure::store::state::WorkspaceStateStore;

/// Every recorded session, in stored order. Empty when the workspace has no
/// `state.json` yet.
///
/// # Errors
///
/// Returns an error when `state.json` exists but cannot be read or parsed.
pub fn list(store: &WorkspaceStateStore) -> Result<Vec<SessionRecord>> {
    Ok(store
        .load()?
        .map(|state| state.sessions)
        .unwrap_or_default())
}

/// The recorded session named `name`, or `None` when there is none.
///
/// # Errors
///
/// Returns an error when `state.json` exists but cannot be read or parsed.
pub fn get(store: &WorkspaceStateStore, name: &str) -> Result<Option<SessionRecord>> {
    Ok(list(store)?.into_iter().find(|s| s.name == name))
}

/// Stamp `now` onto the named session's `last_active` and persist it, returning
/// the touched session — or `None` when no session (or no `state.json`) matches.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or the state cannot be read or
/// written.
pub fn touch(
    store: &WorkspaceStateStore,
    name: &str,
    now: DateTime<Utc>,
) -> Result<Option<SessionRecord>> {
    let _lock = store.lock()?;
    let Some(mut state) = store.load()? else {
        return Ok(None);
    };
    let Some(session) = state.sessions.iter_mut().find(|s| s.name == name) else {
        return Ok(None);
    };
    session.last_active = Some(now);
    let touched = session.clone();
    state.updated_at = now;
    store.save(&state)?;
    Ok(Some(touched))
}

/// Record `session` in the workspace state, replacing any existing record with
/// the same name (an upsert), and persist. Creates `state.json` when it does not
/// exist yet. This maintains only the recorded metadata; the session's git
/// worktrees are the git layer's concern.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or the state cannot be read or
/// written.
pub fn record(
    store: &WorkspaceStateStore,
    session: SessionRecord,
    now: DateTime<Utc>,
) -> Result<()> {
    let _lock = store.lock()?;
    let mut state = store.load()?.unwrap_or_default();
    match state.sessions.iter().position(|s| s.name == session.name) {
        Some(pos) => state.sessions[pos] = session,
        None => state.sessions.push(session),
    }
    state.updated_at = now;
    store.save(&state)
}

/// Remove the recorded session named `name` from the state, returning whether one
/// was removed, and persist when it was. State-only primitive: the session's git
/// worktree is untouched (see [`remove`] for the full teardown).
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or the state cannot be read or
/// written.
pub fn remove_record(store: &WorkspaceStateStore, name: &str, now: DateTime<Utc>) -> Result<bool> {
    let _lock = store.lock()?;
    let Some(mut state) = store.load()? else {
        return Ok(false);
    };
    let before = state.sessions.len();
    state.sessions.retain(|s| s.name != name);
    if state.sessions.len() == before {
        return Ok(false);
    }
    state.updated_at = now;
    store.save(&state)?;
    Ok(true)
}

/// The fields supplied when creating a session. `origin` records who created it;
/// `display_name` / `started_from` are optional.
#[derive(Debug, Clone, Default)]
pub struct NewSession {
    pub name: String,
    pub display_name: Option<String>,
    pub origin: SessionOrigin,
    pub started_from: Option<String>,
}

/// Create a session: add its git worktree, then record it in `state.json`.
///
/// The worktree is a new branch `usagi/<name>` checked out at
/// `<repo_root>/.usagi/sessions/<name>`, branched from the repository's current
/// `HEAD`. The **worktree is created first** — the step most likely to fail (e.g.
/// a branch-name clash) — and only on success is the session recorded. If
/// recording then fails, the just-created worktree is removed (best-effort) so no
/// orphan is left behind, and the original error is returned.
///
/// # Errors
///
/// Returns an error when `git worktree add` fails or the state cannot be written.
pub fn create(
    runner: &dyn GitRunner,
    store: &WorkspaceStateStore,
    repo_root: &Path,
    spec: NewSession,
    now: DateTime<Utc>,
) -> Result<SessionRecord> {
    let branch = format!("usagi/{}", spec.name);
    let root = repo_root
        .join(STATE_DIR)
        .join(SESSIONS_DIR)
        .join(&spec.name);
    add_worktree(runner, repo_root, &root, &branch, None)?;

    let session = SessionRecord {
        name: spec.name,
        display_name: spec.display_name,
        origin: spec.origin,
        started_from: spec.started_from,
        root,
        created_at: now,
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    if let Err(e) = record(store, session.clone(), now) {
        // Roll back the worktree so a failed create leaves nothing behind.
        let _ = remove_worktree(runner, repo_root, &session.root, true);
        return Err(e);
    }
    Ok(session)
}

/// Remove a session: tear down its git worktree, then forget it from `state.json`.
///
/// The **worktree is removed first**; if that fails (e.g. it is dirty and `force`
/// is not set) the error propagates and the record is left intact so the caller
/// can retry with `force`. On success the record is removed. Returns whether a
/// record was removed.
///
/// # Errors
///
/// Returns an error when `git worktree remove` fails, or the state cannot be read
/// or written.
pub fn remove(
    runner: &dyn GitRunner,
    store: &WorkspaceStateStore,
    repo_root: &Path,
    name: &str,
    force: bool,
    now: DateTime<Utc>,
) -> Result<bool> {
    let root = repo_root.join(STATE_DIR).join(SESSIONS_DIR).join(name);
    remove_worktree(runner, repo_root, &root, force)?;
    remove_record(store, name, now)
}

#[cfg(test)]
mod tests {
    use super::{NewSession, create, get, list, record, remove, remove_record, touch};
    use crate::domain::note::Scratchpad;
    use crate::domain::session::{SessionOrigin, SessionRecord};
    use crate::infrastructure::git::testkit::{FakeGit, fail, ok};
    use crate::infrastructure::store::state::WorkspaceStateStore;
    use chrono::{DateTime, TimeZone, Utc};
    use std::fs;

    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, day, 0, 0, 0).unwrap()
    }

    fn session(name: &str) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: format!("/repo/.usagi/sessions/{name}").into(),
            created_at: ts(20),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        }
    }

    fn store() -> (tempfile::TempDir, WorkspaceStateStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        (tmp, store)
    }

    #[test]
    fn list_and_get_are_empty_without_a_state_file() {
        let (_tmp, store) = store();
        assert!(list(&store).unwrap().is_empty());
        assert!(get(&store, "anything").unwrap().is_none());
    }

    #[test]
    fn record_creates_state_then_lists_and_gets_the_session() {
        let (_tmp, store) = store();
        record(&store, session("alpha"), ts(20)).unwrap();

        let all = list(&store).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "alpha");
        assert_eq!(get(&store, "alpha").unwrap().unwrap().name, "alpha");
        assert!(get(&store, "beta").unwrap().is_none());
    }

    #[test]
    fn record_upserts_by_name_and_appends_new_ones() {
        let (_tmp, store) = store();
        record(&store, session("alpha"), ts(20)).unwrap();
        record(&store, session("beta"), ts(20)).unwrap();

        // Re-recording "alpha" with a changed field replaces it in place.
        let mut updated = session("alpha");
        updated.display_name = Some("Alpha!".to_string());
        record(&store, updated, ts(21)).unwrap();

        let all = list(&store).unwrap();
        assert_eq!(all.len(), 2);
        let alpha = all.iter().find(|s| s.name == "alpha").unwrap();
        assert_eq!(alpha.display_name.as_deref(), Some("Alpha!"));
    }

    #[test]
    fn touch_sets_last_active_and_returns_the_session() {
        let (_tmp, store) = store();
        record(&store, session("alpha"), ts(20)).unwrap();

        let touched = touch(&store, "alpha", ts(22)).unwrap().unwrap();
        assert_eq!(touched.last_active, Some(ts(22)));
        // Persisted.
        assert_eq!(
            get(&store, "alpha").unwrap().unwrap().last_active,
            Some(ts(22))
        );
    }

    #[test]
    fn touch_is_none_for_an_unknown_name_or_missing_state() {
        let (_tmp, store) = store();
        // No state file yet.
        assert!(touch(&store, "alpha", ts(22)).unwrap().is_none());
        // State exists but no such session.
        record(&store, session("alpha"), ts(20)).unwrap();
        assert!(touch(&store, "ghost", ts(22)).unwrap().is_none());
    }

    #[test]
    fn remove_record_deletes_a_recorded_session_and_reports_success() {
        let (_tmp, store) = store();
        record(&store, session("alpha"), ts(20)).unwrap();
        record(&store, session("beta"), ts(20)).unwrap();

        assert!(remove_record(&store, "alpha", ts(23)).unwrap());
        let names: Vec<String> = list(&store).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn remove_record_is_false_for_an_unknown_name_or_missing_state() {
        let (_tmp, store) = store();
        // No state file yet.
        assert!(!remove_record(&store, "alpha", ts(23)).unwrap());
        // State exists but no such session.
        record(&store, session("alpha"), ts(20)).unwrap();
        assert!(!remove_record(&store, "ghost", ts(23)).unwrap());
        assert_eq!(list(&store).unwrap().len(), 1);
    }

    fn spec(name: &str) -> NewSession {
        NewSession {
            name: name.to_string(),
            origin: SessionOrigin::Mcp,
            ..Default::default()
        }
    }

    #[test]
    fn create_adds_the_worktree_then_records_the_session() {
        let (tmp, store) = store();
        let repo = tmp.path();
        let git = FakeGit::new(vec![ok("")]); // worktree add succeeds

        let created = create(&git, &store, repo, spec("alpha"), ts(20)).unwrap();

        // The worktree add is scoped correctly: branch usagi/<name>, dest under
        // <repo>/.usagi/sessions/<name>, branched from the current HEAD (no base).
        let dest = repo.join(".usagi/sessions/alpha");
        assert_eq!(
            git.calls.borrow()[0],
            vec![
                "worktree",
                "add",
                "-b",
                "usagi/alpha",
                "--",
                dest.to_str().unwrap(),
            ]
        );
        assert_eq!(created.root, dest);
        assert_eq!(created.origin, SessionOrigin::Mcp);
        // Recorded in state.
        assert_eq!(get(&store, "alpha").unwrap().unwrap().name, "alpha");
    }

    #[test]
    fn create_propagates_a_worktree_add_failure_without_recording() {
        let (tmp, store) = store();
        let git = FakeGit::new(vec![fail("fatal: branch 'usagi/alpha' already exists")]);

        let err = create(&git, &store, tmp.path(), spec("alpha"), ts(20))
            .unwrap_err()
            .to_string();
        assert!(err.contains("git worktree add failed"));
        // Nothing was recorded.
        assert!(list(&store).unwrap().is_empty());
    }

    #[test]
    fn create_rolls_back_the_worktree_when_recording_fails() {
        // Force the state record to fail by making `.usagi` a file, so the store's
        // `create_dir_all` cannot make the directory.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::write(repo.join(".usagi"), "blocker").unwrap();
        let store = WorkspaceStateStore::new(repo);
        // add succeeds, then the rollback remove is invoked.
        let git = FakeGit::new(vec![ok(""), ok("")]);

        assert!(create(&git, &store, repo, spec("alpha"), ts(20)).is_err());

        // Two git calls: the add, then the rollback `worktree remove --force`.
        let calls = git.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][..2], ["worktree", "add"]);
        assert_eq!(
            calls[1],
            vec![
                "worktree",
                "remove",
                "--force",
                "--",
                repo.join(".usagi/sessions/alpha").to_str().unwrap(),
            ]
        );
    }

    #[test]
    fn remove_tears_down_the_worktree_then_forgets_the_record() {
        let (tmp, store) = store();
        let repo = tmp.path();
        let git = FakeGit::new(vec![ok("")]); // worktree add
        create(&git, &store, repo, spec("alpha"), ts(20)).unwrap();

        let git = FakeGit::new(vec![ok("")]); // worktree remove
        assert!(remove(&git, &store, repo, "alpha", false, ts(23)).unwrap());
        assert_eq!(
            git.calls.borrow()[0],
            vec![
                "worktree",
                "remove",
                "--",
                repo.join(".usagi/sessions/alpha").to_str().unwrap(),
            ]
        );
        assert!(list(&store).unwrap().is_empty());
    }

    #[test]
    fn remove_propagates_a_dirty_worktree_failure_and_keeps_the_record() {
        let (tmp, store) = store();
        let repo = tmp.path();
        create(
            &FakeGit::new(vec![ok("")]),
            &store,
            repo,
            spec("alpha"),
            ts(20),
        )
        .unwrap();

        // The worktree remove fails (dirty, no force): the record stays.
        let git = FakeGit::new(vec![fail("fatal: contains modified or untracked files")]);
        assert!(remove(&git, &store, repo, "alpha", false, ts(23)).is_err());
        assert_eq!(list(&store).unwrap().len(), 1);
    }

    #[test]
    fn remove_of_an_unrecorded_session_is_a_noop_returning_false() {
        let (tmp, store) = store();
        // The worktree remove is a no-op (git reports not a working tree), and
        // there is no record to forget.
        let git = FakeGit::new(vec![fail("fatal: 'x' is not a working tree")]);
        assert!(!remove(&git, &store, tmp.path(), "x", false, ts(23)).unwrap());
    }
}

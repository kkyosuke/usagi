//! Session state operations over the repo `state.json` store.
//!
//! These are the git-free part of a session's lifecycle: listing, looking up,
//! touching, and recording/removing a [`SessionRecord`] in the persisted
//! [`WorkspaceState`](crate::domain::workspace_state::WorkspaceState). They
//! read-modify-write `state.json` through the injected
//! [`WorkspaceStateStore`], holding its cross-process lock across each mutation
//! so concurrent writers serialise. Creating a session's actual git worktrees
//! (and tearing them down) belongs to the git layer and is out of scope here;
//! [`record`] / [`remove`] only maintain the recorded state.
//!
//! The clock is passed in (`now`) so these stay clock-free and fully testable.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::session::SessionRecord;
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

/// Remove the recorded session named `name`, returning whether one was removed,
/// and persist when it was. Only the recorded state is touched; tearing down the
/// session's git worktrees is the git layer's concern.
///
/// # Errors
///
/// Returns an error when the lock cannot be taken or the state cannot be read or
/// written.
pub fn remove(store: &WorkspaceStateStore, name: &str, now: DateTime<Utc>) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::{get, list, record, remove, touch};
    use crate::domain::note::Scratchpad;
    use crate::domain::session::{SessionOrigin, SessionRecord};
    use crate::infrastructure::store::state::WorkspaceStateStore;
    use chrono::{DateTime, TimeZone, Utc};

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
    fn remove_deletes_a_recorded_session_and_reports_success() {
        let (_tmp, store) = store();
        record(&store, session("alpha"), ts(20)).unwrap();
        record(&store, session("beta"), ts(20)).unwrap();

        assert!(remove(&store, "alpha", ts(23)).unwrap());
        let names: Vec<String> = list(&store).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn remove_is_false_for_an_unknown_name_or_missing_state() {
        let (_tmp, store) = store();
        // No state file yet.
        assert!(!remove(&store, "alpha", ts(23)).unwrap());
        // State exists but no such session.
        record(&store, session("alpha"), ts(20)).unwrap();
        assert!(!remove(&store, "ghost", ts(23)).unwrap());
        assert_eq!(list(&store).unwrap().len(), 1);
    }
}

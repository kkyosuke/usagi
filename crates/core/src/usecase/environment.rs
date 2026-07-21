//! Per-target environment-variable operations over the repo `state.json` store.
//!
//! Every session (and the workspace **root**) carries an environment: a stable
//! `name -> value` map edited through the Overview `env` command. These are the
//! git-free operations that surface reads and writes through the injected
//! [`WorkspaceStateStore`], the same store the note scratchpad uses.
//!
//! A [`Target`] selects whose environment to touch: a named session or the
//! workspace root. [`set_environment`] holds the store lock across
//! load→edit→save and returns `false` when the target session does not exist.
//! The clock is passed in (`now`) so these stay clock-free and testable.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::workspace_state::WorkspaceState;
use crate::infrastructure::store::state::WorkspaceStateStore;
use crate::usecase::note::Target;

/// The environment map for `target` within `state`, or `None` when a named
/// session does not exist. The root always resolves.
fn environment_of<'a>(
    state: &'a WorkspaceState,
    target: Target<'_>,
) -> Option<&'a BTreeMap<String, String>> {
    match target {
        Target::Root => Some(&state.root_environment),
        Target::Session(name) => state
            .sessions
            .iter()
            .find(|s| s.name == name)
            .map(|s| &s.environment),
    }
}

/// Mutable counterpart of [`environment_of`].
fn environment_of_mut<'a>(
    state: &'a mut WorkspaceState,
    target: Target<'_>,
) -> Option<&'a mut BTreeMap<String, String>> {
    match target {
        Target::Root => Some(&mut state.root_environment),
        Target::Session(name) => state
            .sessions
            .iter_mut()
            .find(|s| s.name == name)
            .map(|s| &mut s.environment),
    }
}

/// Read the target's environment map, or an empty one when there is no
/// `state.json` or the target session does not exist.
///
/// # Errors
///
/// Returns an error when `state.json` cannot be read or parsed.
pub fn environment(
    store: &WorkspaceStateStore,
    target: Target<'_>,
) -> Result<BTreeMap<String, String>> {
    Ok(store
        .load()?
        .as_ref()
        .and_then(|state| environment_of(state, target))
        .cloned()
        .unwrap_or_default())
}

/// Replace the target's whole environment map and persist, stamping `now`.
///
/// The Overview editor sends the complete set on every save, so a save is a
/// wholesale replacement: entries absent from `env` are removed. Returns `false`
/// (without writing) when the target session does not exist.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, read, or written.
pub fn set_environment(
    store: &WorkspaceStateStore,
    target: Target<'_>,
    env: BTreeMap<String, String>,
    now: DateTime<Utc>,
) -> Result<bool> {
    let _lock = store.lock()?;
    let mut state = store.load()?.unwrap_or_default();
    let Some(slot) = environment_of_mut(&mut state, target) else {
        return Ok(false);
    };
    *slot = env;
    state.updated_at = now;
    store.save(&state)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{environment, set_environment};
    use crate::domain::note::Scratchpad;
    use crate::domain::session::{SessionOrigin, SessionRecord};
    use crate::domain::workspace_state::WorkspaceState;
    use crate::infrastructure::store::state::WorkspaceStateStore;
    use crate::usecase::note::Target;
    use chrono::{DateTime, TimeZone, Utc};
    use std::collections::BTreeMap;

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
            environment: BTreeMap::new(),
        }
    }

    fn store_with_alpha() -> (tempfile::TempDir, WorkspaceStateStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        let state = WorkspaceState {
            sessions: vec![session("alpha")],
            updated_at: ts(20),
            ..Default::default()
        };
        store.save(&state).unwrap();
        (tmp, store)
    }

    fn empty_store() -> (tempfile::TempDir, WorkspaceStateStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        (tmp, store)
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn root_environment_set_get_and_replace() {
        // The root always resolves; a mutation on an empty store creates state.
        let (_tmp, store) = empty_store();
        assert!(environment(&store, Target::Root).unwrap().is_empty());

        assert!(
            set_environment(&store, Target::Root, env(&[("A", "1"), ("B", "2")]), ts(21)).unwrap()
        );
        assert_eq!(
            environment(&store, Target::Root).unwrap(),
            env(&[("A", "1"), ("B", "2")])
        );

        // A save is a wholesale replacement: dropping "B" and editing "A".
        assert!(set_environment(&store, Target::Root, env(&[("A", "9")]), ts(22)).unwrap());
        assert_eq!(
            environment(&store, Target::Root).unwrap(),
            env(&[("A", "9")])
        );

        // An empty set clears it.
        assert!(set_environment(&store, Target::Root, BTreeMap::new(), ts(23)).unwrap());
        assert!(environment(&store, Target::Root).unwrap().is_empty());
    }

    #[test]
    fn session_environment_round_trips() {
        let (_tmp, store) = store_with_alpha();
        let target = Target::Session("alpha");
        assert!(environment(&store, target).unwrap().is_empty());

        assert!(set_environment(&store, target, env(&[("TOKEN", "xyz")]), ts(21)).unwrap());
        assert_eq!(
            environment(&store, target).unwrap(),
            env(&[("TOKEN", "xyz")])
        );
        // The root is independent of the session's environment.
        assert!(environment(&store, Target::Root).unwrap().is_empty());
    }

    #[test]
    fn set_returns_false_for_an_unknown_session() {
        let (_tmp, store) = store_with_alpha();
        let ghost = Target::Session("ghost");
        assert!(!set_environment(&store, ghost, env(&[("A", "1")]), ts(21)).unwrap());
        // Reads of an absent target are empty rather than an error.
        assert!(environment(&store, ghost).unwrap().is_empty());
    }

    #[test]
    fn reads_are_empty_without_a_state_file() {
        let (_tmp, store) = empty_store();
        assert!(environment(&store, Target::Root).unwrap().is_empty());
        assert!(
            environment(&store, Target::Session("alpha"))
                .unwrap()
                .is_empty()
        );
    }
}

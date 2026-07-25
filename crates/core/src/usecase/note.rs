//! Note scratchpad operations over the repo `state.json` store.
//!
//! Every session (and the workspace **root**) carries a [`Scratchpad`]: a
//! free-form note, a todo checklist, and a decision log. These are the git-free
//! operations the note / todo / decision surfaces call — the MCP
//! `session_note_* / session_todo_* / session_decision_*` tools and the TUI —
//! reading and writing that scratchpad through the injected
//! [`WorkspaceStateStore`].
//!
//! A [`Target`] selects whose scratchpad to touch: a named session or the
//! workspace root. Mutations hold the store lock across load→edit→save and
//! return `false` when the target session does not exist. The clock is passed in
//! (`now`) so these stay clock-free and testable.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::domain::note::{Scratchpad, SessionDecision, SessionTodo};
use crate::domain::workspace_state::WorkspaceState;
use crate::infrastructure::store::state::WorkspaceStateStore;

/// Whose scratchpad an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// The workspace root's scratchpad (the `⌂ root` row).
    Root,
    /// The named session's scratchpad.
    Session(&'a str),
}

/// The scratchpad for `target` within `state`, or `None` when a named session
/// does not exist. The root always resolves.
fn scratchpad<'a>(state: &'a WorkspaceState, target: Target<'_>) -> Option<&'a Scratchpad> {
    match target {
        Target::Root => Some(&state.root_notes),
        Target::Session(name) => state
            .sessions
            .iter()
            .find(|s| s.name == name)
            .map(|s| &s.notes),
    }
}

/// Mutable counterpart of [`scratchpad`].
fn scratchpad_mut<'a>(
    state: &'a mut WorkspaceState,
    target: Target<'_>,
) -> Option<&'a mut Scratchpad> {
    match target {
        Target::Root => Some(&mut state.root_notes),
        Target::Session(name) => state
            .sessions
            .iter_mut()
            .find(|s| s.name == name)
            .map(|s| &mut s.notes),
    }
}

/// Read the target's scratchpad, or a default (empty) one when there is no
/// `state.json` or the target session does not exist.
fn read(store: &WorkspaceStateStore, target: Target<'_>) -> Result<Scratchpad> {
    Ok(store
        .load()?
        .as_ref()
        .and_then(|state| scratchpad(state, target))
        .cloned()
        .unwrap_or_default())
}

/// Apply `edit` to the target's scratchpad and persist, stamping `now`. Returns
/// `false` (without writing) when the target session does not exist.
fn mutate(
    store: &WorkspaceStateStore,
    target: Target<'_>,
    now: DateTime<Utc>,
    edit: impl FnOnce(&mut Scratchpad) -> bool,
) -> Result<bool> {
    let _lock = store.lock()?;
    let mut state = store.load()?.unwrap_or_default();
    let Some(pad) = scratchpad_mut(&mut state, target) else {
        return Ok(false);
    };
    if !edit(pad) {
        return Ok(false);
    }
    state.updated_at = now;
    store.save(&state)?;
    Ok(true)
}

/// The target's free-form note, or `None` when unset (or the target is absent).
///
/// # Errors
///
/// Returns an error when `state.json` cannot be read or parsed.
pub fn note(store: &WorkspaceStateStore, target: Target<'_>) -> Result<Option<String>> {
    Ok(read(store, target)?.note.filter(|n| !n.is_empty()))
}

/// Set the target's note, or clear it when `note` is empty. Returns `false` when
/// the target session does not exist.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, read, or written.
pub fn set_note(
    store: &WorkspaceStateStore,
    target: Target<'_>,
    note: &str,
    now: DateTime<Utc>,
) -> Result<bool> {
    mutate(store, target, now, |pad| {
        pad.note = if note.is_empty() {
            None
        } else {
            Some(note.to_owned())
        };
        true
    })
}

/// The target's todo checklist (empty when the target is absent or has none).
///
/// # Errors
///
/// Returns an error when `state.json` cannot be read or parsed.
pub fn todos(store: &WorkspaceStateStore, target: Target<'_>) -> Result<Vec<SessionTodo>> {
    Ok(read(store, target)?.todos)
}

/// Append a todo with `text` to the target's checklist. Returns `false` when the
/// target session does not exist.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, read, or written.
pub fn add_todo(
    store: &WorkspaceStateStore,
    target: Target<'_>,
    text: &str,
    now: DateTime<Utc>,
) -> Result<bool> {
    mutate(store, target, now, |pad| {
        pad.todos.push(SessionTodo::new(text));
        true
    })
}

/// Update the todo at `index`: set its `done` and/or `text` when provided.
/// Returns `false` when the target session does not exist or the index is out of
/// range.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, read, or written.
pub fn update_todo(
    store: &WorkspaceStateStore,
    target: Target<'_>,
    index: usize,
    done: Option<bool>,
    text: Option<String>,
    now: DateTime<Utc>,
) -> Result<bool> {
    mutate(store, target, now, |pad| {
        let Some(todo) = pad.todos.get_mut(index) else {
            return false;
        };
        if let Some(done) = done {
            todo.done = done;
        }
        if let Some(text) = text {
            todo.text = text;
        }
        true
    })
}

/// Remove the todo at `index`. Returns `false` when the target session does not
/// exist or the index is out of range.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, read, or written.
pub fn remove_todo(
    store: &WorkspaceStateStore,
    target: Target<'_>,
    index: usize,
    now: DateTime<Utc>,
) -> Result<bool> {
    mutate(store, target, now, |pad| {
        if index >= pad.todos.len() {
            return false;
        }
        pad.todos.remove(index);
        true
    })
}

/// The target's decision log (empty when the target is absent or has none).
///
/// # Errors
///
/// Returns an error when `state.json` cannot be read or parsed.
pub fn decisions(store: &WorkspaceStateStore, target: Target<'_>) -> Result<Vec<SessionDecision>> {
    Ok(read(store, target)?.decisions)
}

/// Append a decision recorded at `now` with `text` to the target's log. Returns
/// `false` when the target session does not exist.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, read, or written.
pub fn log_decision(
    store: &WorkspaceStateStore,
    target: Target<'_>,
    text: &str,
    now: DateTime<Utc>,
) -> Result<bool> {
    mutate(store, target, now, |pad| {
        pad.decisions
            .push(SessionDecision::new(now, text.to_owned()));
        true
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Target, add_todo, decisions, log_decision, note, remove_todo, set_note, todos, update_todo,
    };
    use crate::domain::note::Scratchpad;
    use crate::domain::session::{SessionOrigin, SessionRecord};
    use crate::domain::workspace_state::WorkspaceState;
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

    /// A store seeded with one session "alpha" so session-targeted ops resolve.
    fn store_with_alpha() -> (tempfile::TempDir, WorkspaceStateStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        let state = WorkspaceState {
            sessions: vec![session("alpha")],
            root_notes: Scratchpad::default(),
            updated_at: ts(20),
        };
        store.save(&state).unwrap();
        (tmp, store)
    }

    fn empty_store() -> (tempfile::TempDir, WorkspaceStateStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        (tmp, store)
    }

    #[test]
    fn note_set_get_and_clear_on_a_session() {
        let (_tmp, store) = store_with_alpha();
        let target = Target::Session("alpha");
        assert_eq!(note(&store, target).unwrap(), None);

        assert!(set_note(&store, target, "hello", ts(21)).unwrap());
        assert_eq!(note(&store, target).unwrap().as_deref(), Some("hello"));

        // Empty string clears it.
        assert!(set_note(&store, target, "", ts(22)).unwrap());
        assert_eq!(note(&store, target).unwrap(), None);
    }

    #[test]
    fn note_and_todos_work_on_the_root_and_create_state() {
        // The root always resolves; a mutation on an empty store creates state.
        let (_tmp, store) = empty_store();
        assert!(set_note(&store, Target::Root, "root memo", ts(21)).unwrap());
        assert_eq!(
            note(&store, Target::Root).unwrap().as_deref(),
            Some("root memo")
        );
        assert!(add_todo(&store, Target::Root, "triage", ts(21)).unwrap());
        assert_eq!(todos(&store, Target::Root).unwrap().len(), 1);
    }

    #[test]
    fn mutations_return_false_for_an_unknown_session() {
        let (_tmp, store) = store_with_alpha();
        let ghost = Target::Session("ghost");
        assert!(!set_note(&store, ghost, "x", ts(21)).unwrap());
        assert!(!add_todo(&store, ghost, "x", ts(21)).unwrap());
        assert!(!log_decision(&store, ghost, "x", ts(21)).unwrap());
        assert!(!update_todo(&store, ghost, 0, Some(true), None, ts(21)).unwrap());
        assert!(!remove_todo(&store, ghost, 0, ts(21)).unwrap());
        // Reads of an absent target are empty rather than an error.
        assert_eq!(note(&store, ghost).unwrap(), None);
        assert!(todos(&store, ghost).unwrap().is_empty());
        assert!(decisions(&store, ghost).unwrap().is_empty());
    }

    #[test]
    fn todo_add_update_and_remove() {
        let (_tmp, store) = store_with_alpha();
        let target = Target::Session("alpha");
        add_todo(&store, target, "first", ts(21)).unwrap();
        add_todo(&store, target, "second", ts(21)).unwrap();

        // Check off #0 and rename #1.
        assert!(update_todo(&store, target, 0, Some(true), None, ts(22)).unwrap());
        assert!(update_todo(&store, target, 1, None, Some("renamed".to_string()), ts(22)).unwrap());
        let list = todos(&store, target).unwrap();
        assert!(list[0].done);
        assert_eq!(list[1].text, "renamed");

        // Out-of-range update / remove report false.
        assert!(!update_todo(&store, target, 9, Some(true), None, ts(22)).unwrap());
        assert!(!remove_todo(&store, target, 9, ts(22)).unwrap());

        // Remove #0 leaves the renamed one.
        assert!(remove_todo(&store, target, 0, ts(23)).unwrap());
        let list = todos(&store, target).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].text, "renamed");
    }

    #[test]
    fn decisions_are_appended_with_the_supplied_time() {
        let (_tmp, store) = store_with_alpha();
        let target = Target::Session("alpha");
        assert!(decisions(&store, target).unwrap().is_empty());

        assert!(log_decision(&store, target, "chose a trait", ts(21)).unwrap());
        assert!(log_decision(&store, target, "then a store", ts(22)).unwrap());
        let log = decisions(&store, target).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].at, ts(21));
        assert_eq!(log[0].text, "chose a trait");
        assert_eq!(log[1].at, ts(22));
    }

    #[test]
    fn reads_are_empty_without_a_state_file() {
        let (_tmp, store) = empty_store();
        assert_eq!(note(&store, Target::Root).unwrap(), None);
        assert!(todos(&store, Target::Root).unwrap().is_empty());
        assert!(decisions(&store, Target::Root).unwrap().is_empty());
        // Target equality derive is exercised here too.
        assert_eq!(Target::Session("a"), Target::Session("a"));
        assert_ne!(Target::Root, Target::Session("a"));
    }
}

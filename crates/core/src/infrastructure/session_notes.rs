//! Shared session scratchpad storage and one-time legacy import.

use std::path::Path;

use anyhow::Result;

use crate::domain::id::SessionId;
use crate::domain::note::Scratchpad;
use crate::infrastructure::store::state::WorkspaceStateStore;
use crate::usecase::note;

/// Read the canonical workspace scratchpad for one daemon-resolved session.
/// Existing workspace annotations take precedence over old worktree-local
/// notes. Import clears transferred workspace annotations, and an empty canonical
/// entry prevents a cleared note from being imported again.
///
/// # Errors
/// Returns an error when either store cannot be read or migration cannot persist.
pub fn load(workspace: &Path, id: SessionId, name: &str, worktree: &Path) -> Result<Scratchpad> {
    let store = WorkspaceStateStore::new(workspace);
    let state = store.load()?.unwrap_or_default();
    if let Some(notes) = state.session_notes.get(&id) {
        return Ok(notes.clone());
    }
    let legacy = state
        .sessions
        .iter()
        .find(|record| record.name == name)
        .map(|record| record.notes.clone())
        .filter(|notes| !notes.is_empty());
    let local = note::read(&WorkspaceStateStore::new(worktree), note::Target::Root)?;
    let mut legacy = legacy.unwrap_or_default();
    if legacy.note.is_none() {
        legacy.note = local.note;
    }
    for todo in local.todos {
        if !legacy.todos.contains(&todo) {
            legacy.todos.push(todo);
        }
    }
    for decision in local.decisions {
        if !legacy.decisions.contains(&decision) {
            legacy.decisions.push(decision);
        }
    }
    note::initialize_session(&store, id, name, &legacy)?;
    note::read(&store, note::Target::Managed(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::note::{SessionDecision, SessionTodo};
    use crate::domain::session::{SessionOrigin, SessionRecord};
    use crate::domain::workspace_state::WorkspaceState;

    #[test]
    fn imports_both_legacy_stores_and_preserves_explicit_clear_and_session_identity() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path();
        let worktree = workspace.join("child");
        let id = SessionId::new();
        let other = SessionId::new();
        let store = WorkspaceStateStore::new(workspace);
        let local = WorkspaceStateStore::new(&worktree);
        let todo = SessionTodo::new("test it");
        let decision = SessionDecision::new(chrono::Utc::now(), "keep the boundary");
        let pad = Scratchpad {
            note: Some("legacy MCP".into()),
            todos: vec![todo.clone()],
            decisions: vec![decision.clone()],
        };
        local
            .save(&WorkspaceState {
                root_notes: pad.clone(),
                ..Default::default()
            })
            .unwrap();
        store
            .save(&WorkspaceState {
                sessions: vec![SessionRecord {
                    name: "child".into(),
                    display_name: None,
                    origin: SessionOrigin::Human,
                    started_from: None,
                    root: worktree.clone(),
                    created_at: chrono::Utc::now(),
                    last_active: None,
                    notes: Scratchpad {
                        note: Some("legacy TUI".into()),
                        todos: vec![todo],
                        decisions: vec![decision],
                    },
                    prs: Vec::new(),
                }],
                ..Default::default()
            })
            .unwrap();
        let imported = load(workspace, id, "child", &worktree).unwrap();
        assert_eq!(imported.note.as_deref(), Some("legacy TUI"));
        assert_eq!(imported.todos, pad.todos);
        assert_eq!(imported.decisions, pad.decisions);
        assert!(store.load().unwrap().unwrap().sessions[0].notes.is_empty());
        assert!(note::set_note(&store, note::Target::Managed(id), "", chrono::Utc::now()).unwrap());
        assert_eq!(load(workspace, id, "child", &worktree).unwrap().note, None);
        assert!(
            !note::set_note(
                &store,
                note::Target::Managed(other),
                "stale",
                chrono::Utc::now()
            )
            .unwrap()
        );
        // A recreated session has a fresh worktree and ID, even with the same name.
        let fresh = workspace.join("fresh-child");
        assert!(load(workspace, other, "child", &fresh).unwrap().is_empty());
        assert_eq!(
            note::read(&store, note::Target::Managed(id))
                .unwrap()
                .todos
                .len(),
            1
        );
        note::initialize_session(&store, id, "child", &pad).unwrap();
        assert_eq!(note::note(&store, note::Target::Managed(id)).unwrap(), None);
    }

    #[test]
    fn imports_worktree_notes_and_lists_when_workspace_has_no_legacy_record() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("child");
        let pad = Scratchpad {
            note: Some("own note".into()),
            todos: vec![SessionTodo::new("next")],
            decisions: vec![SessionDecision::new(chrono::Utc::now(), "why")],
        };
        WorkspaceStateStore::new(&worktree)
            .save(&WorkspaceState {
                root_notes: pad.clone(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            load(temp.path(), SessionId::new(), "child", &worktree).unwrap(),
            pad
        );
    }

    #[test]
    fn read_errors_are_reported_without_importing_or_overwriting_data() {
        let temp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(temp.path());
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.state_path(), "malformed").unwrap();
        assert!(
            load(
                temp.path(),
                SessionId::new(),
                "child",
                &temp.path().join("child")
            )
            .is_err()
        );
        std::fs::remove_file(store.state_path()).unwrap();
        let worktree = temp.path().join("child");
        let local = WorkspaceStateStore::new(&worktree);
        std::fs::create_dir_all(local.dir()).unwrap();
        std::fs::write(local.state_path(), "malformed").unwrap();
        assert!(load(temp.path(), SessionId::new(), "child", &worktree).is_err());
        assert!(store.load().unwrap().is_none());
    }
}

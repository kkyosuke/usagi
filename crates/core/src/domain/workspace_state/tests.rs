use super::WorkspaceState;
use crate::domain::note::{Scratchpad, SessionTodo};
use crate::domain::session::{SessionOrigin, SessionRecord};
use chrono::{TimeZone, Utc};

fn session(name: &str) -> SessionRecord {
    let ts = Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).unwrap();
    SessionRecord {
        name: name.to_string(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: format!("/repo/.usagi/sessions/{name}").into(),
        created_at: ts,
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
        environment: std::collections::BTreeMap::new(),
    }
}

#[test]
fn new_and_default_are_empty() {
    let state = WorkspaceState::new();
    assert!(state.sessions.is_empty());
    assert!(state.root_notes.is_empty());
    // Default defers to new(); both start empty.
    let def = WorkspaceState::default();
    assert!(def.sessions.is_empty());
    assert!(def.root_notes.is_empty());
}

#[test]
fn empty_state_omits_sessions_and_root_notes() {
    let state = WorkspaceState::new();
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("sessions"), "{json}");
    assert!(!json.contains("root_notes"), "{json}");
    assert!(json.contains("updated_at"));
}

#[test]
fn populated_state_round_trips_through_json() {
    let ts = Utc.with_ymd_and_hms(2026, 6, 20, 1, 0, 0).unwrap();
    let state = WorkspaceState {
        sessions: vec![session("alpha"), session("beta")],
        root_notes: Scratchpad {
            note: Some("root memo".to_string()),
            todos: vec![SessionTodo::new("triage")],
            decisions: Vec::new(),
        },
        root_environment: std::collections::BTreeMap::new(),
        updated_at: ts,
    };

    let json = serde_json::to_string(&state).unwrap();
    assert!(json.contains("\"sessions\""));
    assert!(json.contains("root_notes"));

    let back: WorkspaceState = serde_json::from_str(&json).unwrap();
    assert_eq!(back, state);
    // Exercise the derived Clone / Debug.
    assert_eq!(state.clone(), state);
    assert!(format!("{state:?}").contains("alpha"));
}

#[test]
fn an_older_file_without_optional_keys_loads() {
    // A state.json with only `updated_at` (older / minimal) loads with empty
    // sessions and an empty root scratchpad.
    let restored: WorkspaceState =
        serde_json::from_str(r#"{"updated_at":"2026-06-13T05:01:18.659149Z"}"#).unwrap();
    assert!(restored.sessions.is_empty());
    assert!(restored.root_notes.is_empty());
}

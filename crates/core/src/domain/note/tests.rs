use super::{Scratchpad, SessionDecision, SessionTodo};
use chrono::{TimeZone, Utc};

#[test]
fn todo_new_is_unchecked_and_omits_done_when_false() {
    let todo = SessionTodo::new("write tests");
    assert_eq!(todo.text, "write tests");
    assert!(!todo.done);

    let json = serde_json::to_string(&todo).unwrap();
    assert!(!json.contains("done"), "{json}");

    // A checked todo keeps the `done` key and round-trips.
    let done = SessionTodo {
        text: "ship".to_string(),
        done: true,
    };
    let back: SessionTodo = serde_json::from_str(&serde_json::to_string(&done).unwrap()).unwrap();
    assert_eq!(back, done);
    // Default is an empty, unchecked todo.
    assert_eq!(SessionTodo::default(), SessionTodo::new(""));
}

#[test]
fn decision_new_carries_time_and_text_and_round_trips() {
    let at = Utc.with_ymd_and_hms(2026, 6, 20, 12, 0, 0).unwrap();
    let d = SessionDecision::new(at, "use a trait");
    assert_eq!(d.at, at);
    assert_eq!(d.text, "use a trait");
    let back: SessionDecision = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
    assert_eq!(back, d);
}

#[test]
fn empty_scratchpad_is_empty_and_serializes_to_an_empty_object() {
    let pad = Scratchpad::default();
    assert!(pad.is_empty());
    assert_eq!(pad.note(), None);
    assert!(pad.todos().is_empty());
    assert!(pad.decisions().is_empty());
    // All sections are omitted when empty.
    assert_eq!(serde_json::to_string(&pad).unwrap(), "{}");
}

#[test]
fn populated_scratchpad_reports_its_sections_and_round_trips() {
    let at = Utc.with_ymd_and_hms(2026, 6, 20, 12, 0, 0).unwrap();
    let pad = Scratchpad {
        note: Some("scratch".to_string()),
        todos: vec![SessionTodo::new("a"), SessionTodo::new("b")],
        decisions: vec![SessionDecision::new(at, "why")],
    };
    assert!(!pad.is_empty());
    assert_eq!(pad.note(), Some("scratch"));
    assert_eq!(pad.todos().len(), 2);
    assert_eq!(pad.decisions().len(), 1);

    let json = serde_json::to_string(&pad).unwrap();
    let back: Scratchpad = serde_json::from_str(&json).unwrap();
    assert_eq!(back, pad);
    // Exercise the derived Clone / Debug.
    assert_eq!(pad.clone(), pad);
    assert!(format!("{pad:?}").contains("scratch"));
}

#[test]
fn scratchpad_is_non_empty_when_only_one_section_is_set() {
    let only_note = Scratchpad {
        note: Some("n".to_string()),
        ..Default::default()
    };
    let only_todos = Scratchpad {
        todos: vec![SessionTodo::new("t")],
        ..Default::default()
    };
    let only_decisions = Scratchpad {
        decisions: vec![SessionDecision::new(Utc::now(), "d")],
        ..Default::default()
    };
    assert!(!only_note.is_empty());
    assert!(!only_todos.is_empty());
    assert!(!only_decisions.is_empty());
}

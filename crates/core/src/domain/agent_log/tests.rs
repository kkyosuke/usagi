use super::{TraceCategory, TraceEvent};
use chrono::Utc;

#[test]
fn now_stamps_the_current_time_with_no_detail() {
    let before = Utc::now();
    let event = TraceEvent::now(TraceCategory::Cli, "doctor");
    let after = Utc::now();
    assert_eq!(event.category, TraceCategory::Cli);
    assert_eq!(event.action, "doctor");
    assert_eq!(event.detail, None);
    assert!(event.recorded_at >= before && event.recorded_at <= after);
}

#[test]
fn with_detail_attaches_the_specifics() {
    let event = TraceEvent::now(TraceCategory::Session, "create").with_detail("feature-x");
    assert_eq!(event.detail.as_deref(), Some("feature-x"));
    // Exercise the derived Clone / Debug.
    assert_eq!(event.clone(), event);
    assert!(format!("{event:?}").contains("create"));
}

#[test]
fn round_trips_through_json() {
    let event = TraceEvent::now(TraceCategory::Mcp, "issue_create").with_detail("ok");
    let line = serde_json::to_string(&event).unwrap();
    let parsed: TraceEvent = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed, event);
}

#[test]
fn category_serializes_lowercase_for_every_variant() {
    for (cat, token) in [
        (TraceCategory::Cli, "cli"),
        (TraceCategory::Tui, "tui"),
        (TraceCategory::Session, "session"),
        (TraceCategory::Mcp, "mcp"),
    ] {
        let event = TraceEvent::now(cat, "x");
        let value: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["category"], token);
    }
}

#[test]
fn absent_detail_is_omitted_from_the_json_line() {
    let event = TraceEvent::now(TraceCategory::Cli, "status");
    let line = serde_json::to_string(&event).unwrap();
    assert!(!line.contains("detail"), "{line}");
}

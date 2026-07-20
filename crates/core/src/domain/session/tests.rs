use super::{SessionOrigin, SessionRecord};
use crate::domain::note::{Scratchpad, SessionTodo};
use crate::domain::pullrequest::PrLink;
use chrono::{TimeZone, Utc};

fn sample() -> SessionRecord {
    let ts = Utc.with_ymd_and_hms(2026, 6, 20, 1, 2, 3).unwrap();
    SessionRecord {
        name: "feature-x".to_string(),
        display_name: None,
        origin: SessionOrigin::Mcp,
        started_from: Some("root".to_string()),
        root: "/repo/.usagi/sessions/feature-x".into(),
        created_at: ts,
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
        environment: std::collections::BTreeMap::new(),
    }
}

#[test]
fn origin_tokens_round_trip_and_match_serde() {
    for origin in [
        SessionOrigin::Unknown,
        SessionOrigin::Human,
        SessionOrigin::Mcp,
    ] {
        assert_eq!(origin.to_string(), origin.as_str());
        // `as_str` and the serde `rename_all` derive spell the token independently.
        assert_eq!(serde_json::to_value(origin).unwrap(), origin.as_str());
    }
}

#[test]
fn origin_default_is_unknown_and_is_unknown_reports_it() {
    assert_eq!(SessionOrigin::default(), SessionOrigin::Unknown);
    assert!(SessionOrigin::Unknown.is_unknown());
    assert!(!SessionOrigin::Human.is_unknown());
}

#[test]
fn origin_degrades_an_unrecognised_token_to_unknown() {
    // A token a newer usagi might write degrades to Unknown rather than failing.
    let parsed: SessionOrigin = serde_json::from_str("\"robot\"").unwrap();
    assert_eq!(parsed, SessionOrigin::Unknown);
    // The known tokens still parse to their variant.
    assert_eq!(
        serde_json::from_str::<SessionOrigin>("\"human\"").unwrap(),
        SessionOrigin::Human
    );
}

#[test]
fn display_label_prefers_display_name_then_falls_back_to_name() {
    let mut s = sample();
    assert_eq!(s.display_label(), "feature-x");
    s.display_name = Some("Feature X".to_string());
    assert_eq!(s.display_label(), "Feature X");
}

#[test]
fn last_active_or_created_falls_back_to_created_at() {
    let mut s = sample();
    assert_eq!(s.last_active_or_created(), s.created_at);
    let later = Utc.with_ymd_and_hms(2026, 6, 21, 0, 0, 0).unwrap();
    s.last_active = Some(later);
    assert_eq!(s.last_active_or_created(), later);
}

#[test]
fn record_round_trips_through_json_and_omits_defaults() {
    let s = sample();
    let json = serde_json::to_string(&s).unwrap();
    // Unset optional fields are omitted; the set origin is present.
    assert!(!json.contains("display_name"));
    assert!(!json.contains("last_active"));
    assert!(json.contains("\"origin\":\"mcp\""));
    assert!(json.contains("\"started_from\":\"root\""));

    let back: SessionRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
    // Exercise the derived Clone / Debug.
    assert_eq!(s.clone(), s);
    assert!(format!("{s:?}").contains("feature-x"));
}

#[test]
fn empty_notes_and_prs_are_omitted_but_populated_ones_round_trip() {
    // Empty scratchpad and PR list are omitted from the file.
    let empty = sample();
    let json = serde_json::to_string(&empty).unwrap();
    assert!(!json.contains("notes"), "{json}");
    assert!(!json.contains("prs"), "{json}");

    // Populated notes and PRs persist and round-trip.
    let mut s = sample();
    s.notes = Scratchpad {
        note: Some("wip".to_string()),
        todos: vec![SessionTodo::new("do it")],
        decisions: Vec::new(),
    };
    s.prs = vec![PrLink::new(7, "https://x/pull/7")];
    let json = serde_json::to_string(&s).unwrap();
    assert!(json.contains("notes"));
    assert!(json.contains("\"prs\""));
    let back: SessionRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, s);
}

#[test]
fn unknown_origin_is_omitted_from_the_json() {
    let mut s = sample();
    s.origin = SessionOrigin::Unknown;
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("origin"));
    // It round-trips back to Unknown via the default.
    assert_eq!(
        serde_json::from_str::<SessionRecord>(&json).unwrap().origin,
        SessionOrigin::Unknown
    );
}

use super::{DaemonProcessObservation, DaemonRecord, DaemonState, classify};

#[test]
fn new_records_pid_and_stamps_start_time() {
    let before = chrono::Utc::now();
    let record = DaemonRecord::new(4321);
    let after = chrono::Utc::now();
    assert_eq!(record.pid, 4321);
    assert_eq!(record.process_start_identity, None);
    assert!(!record.has_process_identity());
    // `started_at` is stamped from `Utc::now()` inside `new`, so it falls within
    // the window around the call.
    assert!(record.started_at >= before && record.started_at <= after);
    // Exercise the derived Clone / PartialEq / Debug.
    assert_eq!(record.clone(), record);
    assert!(format!("{record:?}").contains("4321"));
}

#[test]
fn identified_record_carries_non_empty_process_identity() {
    let record = DaemonRecord::identified(4321, "macos:100:200");
    assert_eq!(
        record.process_start_identity.as_deref(),
        Some("macos:100:200")
    );
    assert!(record.has_process_identity());
    assert!(!DaemonRecord::identified(4321, "").has_process_identity());
}

#[test]
fn daemon_record_round_trips_through_json() {
    let record = DaemonRecord::identified(4321, "linux:12345");
    let json = serde_json::to_string(&record).unwrap();
    let back: DaemonRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, record);
}

#[test]
fn legacy_record_without_identity_deserializes_as_unknown() {
    let back: DaemonRecord =
        serde_json::from_str(r#"{"pid":4321,"started_at":"2026-07-23T00:00:00Z"}"#).unwrap();
    assert_eq!(back.process_start_identity, None);
    assert!(!back.has_process_identity());
}

#[test]
fn classify_reports_absent_when_no_record() {
    for observation in [
        DaemonProcessObservation::Exact,
        DaemonProcessObservation::Gone,
        DaemonProcessObservation::IdentityMismatch,
        DaemonProcessObservation::Unknown,
    ] {
        assert_eq!(classify(None, observation), DaemonState::Absent);
    }
}

#[test]
fn classify_reports_alive_only_for_exact_owner() {
    let record = DaemonRecord::new(4321);
    assert_eq!(
        classify(Some(&record), DaemonProcessObservation::Exact),
        DaemonState::Alive
    );
}

#[test]
fn classify_reports_stale_when_record_but_process_gone() {
    let record = DaemonRecord::new(4321);
    assert_eq!(
        classify(Some(&record), DaemonProcessObservation::Gone),
        DaemonState::Stale
    );
}

#[test]
fn classify_reports_unverified_for_mismatch_or_unknown() {
    let record = DaemonRecord::new(4321);
    for observation in [
        DaemonProcessObservation::IdentityMismatch,
        DaemonProcessObservation::Unknown,
    ] {
        assert_eq!(
            classify(Some(&record), observation),
            DaemonState::Unverified
        );
    }
}

#[test]
fn daemon_state_derives_are_exercised() {
    // Cover the derived Clone / Copy / PartialEq / Debug on DaemonState.
    let state = DaemonState::Alive;
    assert_eq!({ state }, state);
    assert_ne!(state, DaemonState::Stale);
    assert!(format!("{state:?}").contains("Alive"));
    let observation = DaemonProcessObservation::IdentityMismatch;
    assert_eq!({ observation }, observation);
    assert!(format!("{observation:?}").contains("Mismatch"));
}

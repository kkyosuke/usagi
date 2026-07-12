use super::{DaemonRecord, DaemonState, classify};

#[test]
fn new_records_pid_and_stamps_start_time() {
    let before = chrono::Utc::now();
    let record = DaemonRecord::new(4321);
    let after = chrono::Utc::now();
    assert_eq!(record.pid, 4321);
    // `started_at` is stamped from `Utc::now()` inside `new`, so it falls within
    // the window around the call.
    assert!(record.started_at >= before && record.started_at <= after);
    // Exercise the derived Clone / PartialEq / Debug.
    assert_eq!(record.clone(), record);
    assert!(format!("{record:?}").contains("4321"));
}

#[test]
fn daemon_record_round_trips_through_json() {
    let record = DaemonRecord::new(4321);
    let json = serde_json::to_string(&record).unwrap();
    let back: DaemonRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, record);
}

#[test]
fn classify_reports_absent_when_no_record() {
    // No record: the liveness flag is irrelevant, so both values yield Absent.
    assert_eq!(classify(None, false), DaemonState::Absent);
    assert_eq!(classify(None, true), DaemonState::Absent);
}

#[test]
fn classify_reports_alive_when_record_and_process_live() {
    let record = DaemonRecord::new(4321);
    assert_eq!(classify(Some(&record), true), DaemonState::Alive);
}

#[test]
fn classify_reports_stale_when_record_but_process_gone() {
    let record = DaemonRecord::new(4321);
    assert_eq!(classify(Some(&record), false), DaemonState::Stale);
}

#[test]
fn daemon_state_derives_are_exercised() {
    // Cover the derived Clone / Copy / PartialEq / Debug on DaemonState.
    let state = DaemonState::Alive;
    assert_eq!({ state }, state);
    assert_ne!(state, DaemonState::Stale);
    assert!(format!("{state:?}").contains("Alive"));
}

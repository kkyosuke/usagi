use super::DaemonRecord;

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

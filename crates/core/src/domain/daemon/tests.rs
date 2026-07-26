use super::{
    DaemonProcessObservation, DaemonRecord, DaemonState, InvalidRecordPid, MAX_RECORD_PID,
    MIN_RECORD_PID, StaleReason, classify, is_record_pid,
};

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
fn a_record_pid_that_cannot_name_a_process_is_rejected_on_the_way_in() {
    // 0 would address the reader's own process group and 1 the init process;
    // above `pid_t::MAX` the value is the wire form of a negative pid, which
    // addresses a process group too. A `-1` literal fails as `u32` before the
    // range check even sees it.
    for pid in [0, 1, MAX_RECORD_PID + 1, u32::MAX] {
        assert!(!is_record_pid(pid), "{pid} must not be a record pid");
        let json = format!(r#"{{"pid":{pid},"started_at":"2026-07-23T00:00:00Z"}}"#);
        let error = serde_json::from_str::<DaemonRecord>(&json).unwrap_err();
        assert!(
            error.to_string().contains("cannot name a process"),
            "{pid}: {error}"
        );
    }
    assert!(
        serde_json::from_str::<DaemonRecord>(r#"{"pid":-1,"started_at":"2026-07-23T00:00:00Z"}"#)
            .is_err()
    );

    for pid in [MIN_RECORD_PID, 4321, MAX_RECORD_PID] {
        assert!(is_record_pid(pid), "{pid} must be a record pid");
        let json = format!(r#"{{"pid":{pid},"started_at":"2026-07-23T00:00:00Z"}}"#);
        assert_eq!(
            serde_json::from_str::<DaemonRecord>(&json).unwrap().pid,
            pid
        );
    }
}

#[test]
fn invalid_record_pid_names_the_value_and_the_accepted_range() {
    let rejected = InvalidRecordPid(1);
    assert_eq!(
        rejected.to_string(),
        format!(
            "daemon record pid 1 cannot name a process (expected {MIN_RECORD_PID}..={MAX_RECORD_PID})"
        )
    );
    // Cover the derived Clone / Copy / PartialEq / Debug.
    assert_eq!({ rejected }, InvalidRecordPid(1));
    assert_ne!(rejected, InvalidRecordPid(0));
    assert!(format!("{rejected:?}").contains("InvalidRecordPid"));
}

#[test]
fn a_malformed_record_is_rejected_without_a_pid_verdict() {
    for json in [
        "not json",
        "{}",
        r#"{"pid":4321}"#,
        r#"{"pid":"4321","started_at":"2026-07-23T00:00:00Z"}"#,
        r#"{"pid":4321,"started_at":"yesterday"}"#,
    ] {
        assert!(
            serde_json::from_str::<DaemonRecord>(json).is_err(),
            "{json}"
        );
    }
}

#[test]
fn classify_decides_every_observation_against_a_present_and_an_absent_record() {
    let record = DaemonRecord::new(4321);
    // Without a record there is nothing to own, whatever the OS reports; with
    // one, `Gone` and `IdentityMismatch` are both proof that the recorded owner
    // incarnation is gone, and only `Unknown` leaves ownership undecided.
    for (observation, expected) in [
        (DaemonProcessObservation::Exact, DaemonState::Alive),
        (
            DaemonProcessObservation::Gone,
            DaemonState::Stale(StaleReason::OwnerGone),
        ),
        (
            DaemonProcessObservation::IdentityMismatch,
            DaemonState::Stale(StaleReason::PidReused),
        ),
        (DaemonProcessObservation::Unknown, DaemonState::Unverified),
    ] {
        assert_eq!(classify(None, observation), DaemonState::Absent);
        assert_eq!(classify(Some(&record), observation), expected);
    }
}

#[test]
fn daemon_state_derives_are_exercised() {
    // Cover the derived Clone / Copy / PartialEq / Debug on DaemonState.
    let state = DaemonState::Alive;
    assert_eq!({ state }, state);
    assert_ne!(state, DaemonState::Stale(StaleReason::OwnerGone));
    assert!(format!("{state:?}").contains("Alive"));
    let reason = StaleReason::PidReused;
    assert_eq!({ reason }, reason);
    assert_ne!(reason, StaleReason::OwnerGone);
    assert!(format!("{reason:?}").contains("PidReused"));
    let observation = DaemonProcessObservation::IdentityMismatch;
    assert_eq!({ observation }, observation);
    assert!(format!("{observation:?}").contains("Mismatch"));
}

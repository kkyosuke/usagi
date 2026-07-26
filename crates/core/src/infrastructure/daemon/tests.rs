use std::cell::RefCell;
use std::io;

use super::{DaemonRecordStore, RecordFile};
use crate::domain::daemon::DaemonRecord;

/// An in-memory [`RecordFile`] standing in for the JSON file on disk.
#[derive(Default)]
struct InMemoryFile {
    contents: RefCell<Option<String>>,
}

impl InMemoryFile {
    fn with(contents: &str) -> Self {
        Self {
            contents: RefCell::new(Some(contents.to_string())),
        }
    }
}

impl RecordFile for InMemoryFile {
    fn read(&self) -> io::Result<Option<String>> {
        Ok(self.contents.borrow().clone())
    }

    fn write(&self, contents: &str) -> io::Result<()> {
        *self.contents.borrow_mut() = Some(contents.to_string());
        Ok(())
    }

    fn remove_if(&self, expected: &str) -> io::Result<bool> {
        let mut contents = self.contents.borrow_mut();
        if contents.as_deref() == Some(expected) {
            *contents = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// A [`RecordFile`] whose every operation fails, to exercise IO error propagation.
struct FailingFile;

impl RecordFile for FailingFile {
    fn read(&self) -> io::Result<Option<String>> {
        Err(io::Error::other("read failed"))
    }

    fn write(&self, _contents: &str) -> io::Result<()> {
        Err(io::Error::other("write failed"))
    }

    fn remove_if(&self, _expected: &str) -> io::Result<bool> {
        Err(io::Error::other("remove failed"))
    }
}

#[test]
fn load_returns_none_when_file_absent() {
    let store = DaemonRecordStore::new(InMemoryFile::default());
    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn save_then_load_round_trips() {
    let store = DaemonRecordStore::new(InMemoryFile::default());
    let record = DaemonRecord::new(4321);
    store.save(&record).unwrap();
    assert_eq!(store.load().unwrap(), Some(record));
}

#[test]
fn save_overwrites_existing_record() {
    let store = DaemonRecordStore::new(InMemoryFile::default());
    store.save(&DaemonRecord::new(4321)).unwrap();
    let latest = DaemonRecord::new(4322);
    store.save(&latest).unwrap();
    assert_eq!(store.load().unwrap(), Some(latest));
}

#[test]
fn registration_refuses_a_pid_that_cannot_name_a_process() {
    let store = DaemonRecordStore::new(InMemoryFile::default());
    for pid in [0, 1, crate::domain::daemon::MAX_RECORD_PID + 1, u32::MAX] {
        let error = store.save(&DaemonRecord::new(pid)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{pid}");
        assert!(error.to_string().contains("cannot name a process"), "{pid}");
        // Nothing was written, so no later reader can act on the value.
        assert_eq!(store.load().unwrap(), None);
    }
}

#[test]
fn load_rejects_a_persisted_pid_that_cannot_name_a_process() {
    // The value can only arrive by corruption or hand-editing, and rejecting it
    // as unreadable keeps it out of every classification and signal path.
    let store = DaemonRecordStore::new(InMemoryFile::with(
        r#"{"pid":1,"process_start_identity":"linux:1","started_at":"2026-07-23T00:00:00Z"}"#,
    ));
    let error = store.load().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("cannot name a process"));
}

#[test]
fn clear_if_removes_only_the_expected_record() {
    let store = DaemonRecordStore::new(InMemoryFile::default());
    let old = DaemonRecord::new(4321);
    let replacement = DaemonRecord {
        pid: old.pid,
        process_start_identity: old.process_start_identity.clone(),
        started_at: old.started_at + chrono::Duration::nanoseconds(1),
    };
    store.save(&replacement).unwrap();
    assert!(!store.clear_if(&old).unwrap());
    assert_eq!(store.load().unwrap(), Some(replacement.clone()));
    assert!(store.clear_if(&replacement).unwrap());
    assert_eq!(store.load().unwrap(), None);
    assert!(!store.clear_if(&replacement).unwrap());
}

#[test]
fn load_reports_invalid_data_on_malformed_json() {
    let store = DaemonRecordStore::new(InMemoryFile::with("not json"));
    let err = store.load().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn store_propagates_file_io_errors() {
    let store = DaemonRecordStore::new(FailingFile);
    assert!(store.load().is_err());
    assert!(store.save(&DaemonRecord::new(4321)).is_err());
    assert!(store.clear_if(&DaemonRecord::new(4321)).is_err());
}

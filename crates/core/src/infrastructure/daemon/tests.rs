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
    store.save(&DaemonRecord::new(1)).unwrap();
    let latest = DaemonRecord::new(2);
    store.save(&latest).unwrap();
    assert_eq!(store.load().unwrap(), Some(latest));
}

#[test]
fn clear_if_removes_only_the_expected_record() {
    let store = DaemonRecordStore::new(InMemoryFile::default());
    let old = DaemonRecord::new(4321);
    let replacement = DaemonRecord {
        pid: old.pid,
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

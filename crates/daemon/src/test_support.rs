//! Test doubles for the injected daemon seams (record file and liveness probe),
//! shared by the usecase and presentation unit tests.

use std::cell::RefCell;
use std::io;

use usagi_core::infrastructure::daemon::{LivenessProbe, RecordFile};

/// An in-memory [`RecordFile`] standing in for `daemon.json` on disk.
#[derive(Default)]
pub struct InMemoryRecordFile {
    contents: RefCell<Option<String>>,
}

impl InMemoryRecordFile {
    /// A file pre-seeded with `contents`, as if a record were already persisted.
    pub fn with(contents: &str) -> Self {
        Self {
            contents: RefCell::new(Some(contents.to_string())),
        }
    }
}

impl RecordFile for InMemoryRecordFile {
    fn read(&self) -> io::Result<Option<String>> {
        Ok(self.contents.borrow().clone())
    }

    fn write(&self, contents: &str) -> io::Result<()> {
        *self.contents.borrow_mut() = Some(contents.to_string());
        Ok(())
    }

    fn remove(&self) -> io::Result<()> {
        *self.contents.borrow_mut() = None;
        Ok(())
    }
}

/// A [`LivenessProbe`] that reports a fixed answer regardless of pid.
pub struct FixedProbe(pub bool);

impl LivenessProbe for FixedProbe {
    fn is_alive(&self, _pid: u32) -> bool {
        self.0
    }
}

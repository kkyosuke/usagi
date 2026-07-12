//! Test doubles for the injected daemon seams (record file and liveness probe),
//! shared by the usecase and presentation unit tests.

use std::cell::RefCell;
use std::io;

use usagi_core::infrastructure::daemon::{LivenessProbe, RecordFile, ShutdownSignal, Terminator};

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

/// A [`Terminator`] that records the pids it is asked to terminate and can be
/// configured to fail, so tests can assert who was signalled and cover the
/// error path.
#[derive(Default)]
pub struct RecordingTerminator {
    fail: bool,
    terminated: RefCell<Vec<u32>>,
}

impl RecordingTerminator {
    /// A terminator whose `terminate` always fails.
    pub fn failing() -> Self {
        Self {
            fail: true,
            terminated: RefCell::new(Vec::new()),
        }
    }

    /// The pids `terminate` was called with, in order.
    pub fn terminated(&self) -> Vec<u32> {
        self.terminated.borrow().clone()
    }
}

impl Terminator for RecordingTerminator {
    fn terminate(&self, pid: u32) -> io::Result<()> {
        self.terminated.borrow_mut().push(pid);
        if self.fail {
            Err(io::Error::other("terminate failed"))
        } else {
            Ok(())
        }
    }
}

/// A [`ShutdownSignal`] that returns immediately, so `serve` runs its
/// register → wait → clear path to completion without blocking.
pub struct ImmediateShutdown;

impl ShutdownSignal for ImmediateShutdown {
    fn wait(&self) -> io::Result<()> {
        Ok(())
    }
}

/// A [`ShutdownSignal`] whose wait fails, to cover the error path.
pub struct FailingShutdown;

impl ShutdownSignal for FailingShutdown {
    fn wait(&self) -> io::Result<()> {
        Err(io::Error::other("wait failed"))
    }
}

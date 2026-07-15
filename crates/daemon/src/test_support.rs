//! Test doubles for the injected daemon seams (record file and liveness probe),
//! shared by the usecase and presentation unit tests.

use std::cell::RefCell;
use std::io;

use usagi_core::domain::daemon::DaemonRecord;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe, RecordFile,
    ShutdownSignal, Sleeper, Terminator,
};

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

/// A [`DaemonReady`] that publishes nothing, for lifecycle tests that do not
/// exercise the endpoint boundary.
pub struct NoopReady;

impl DaemonReady for NoopReady {
    fn publish(&self) -> io::Result<()> {
        Ok(())
    }
}

/// A [`DaemonLauncher`] for `start` tests. When built with [`registering`], it
/// mimics the spawned `serve` writing `pid` into the shared store so the poll
/// finds it; when built with [`idle`], it spawns nothing so the poll times out.
///
/// Both variants are the same type so `start` monomorphizes once across the test
/// suite (distinct launcher types would split coverage across monomorphizations).
///
/// [`registering`]: TestLauncher::registering
/// [`idle`]: TestLauncher::idle
pub struct TestLauncher<'a, F> {
    store: &'a DaemonRecordStore<F>,
    register_pid: Option<u32>,
}

impl<'a, F> TestLauncher<'a, F> {
    /// A launcher that registers `pid` into `store` on launch.
    pub fn registering(store: &'a DaemonRecordStore<F>, pid: u32) -> Self {
        Self {
            store,
            register_pid: Some(pid),
        }
    }

    /// A launcher that spawns nothing, so no record ever appears.
    pub fn idle(store: &'a DaemonRecordStore<F>) -> Self {
        Self {
            store,
            register_pid: None,
        }
    }
}

impl<F: RecordFile> DaemonLauncher for TestLauncher<'_, F> {
    fn launch(&self) -> io::Result<()> {
        if let Some(pid) = self.register_pid {
            self.store.save(&DaemonRecord::new(pid))?;
        }
        Ok(())
    }
}

/// A [`Sleeper`] that does not sleep, so poll loops run instantly under test.
pub struct NoopSleeper;

impl Sleeper for NoopSleeper {
    fn sleep(&self) {}
}

/// An [`InstanceLock`] with a fixed outcome, so `serve` tests exercise acquiring
/// the single-instance lock, being refused, and failing without real locking.
pub enum FakeLock {
    /// The lock is acquired by this process.
    Acquired,
    /// The lock is held by another daemon.
    Held,
    /// Acquiring the lock fails.
    Failing,
}

impl InstanceLock for FakeLock {
    fn acquire(&self) -> io::Result<bool> {
        match self {
            FakeLock::Acquired => Ok(true),
            FakeLock::Held => Ok(false),
            FakeLock::Failing => Err(io::Error::other("lock failed")),
        }
    }
}

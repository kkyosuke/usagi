//! Test doubles for the injected daemon seams (record file and liveness probe),
//! shared by the usecase and presentation unit tests.

use std::cell::{Cell, RefCell};
use std::io;

use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe,
    ProcessIdentitySource, RecordFile, ShutdownSignal, Sleeper, Terminator, WorkspaceFence,
    WorkspaceFenceOutcome,
};

use crate::usecase::serve::{DaemonRecordPort, GenerationAuthority};
use crate::usecase::stop::{StaleCleanup, StaleDaemonCleanup};

/// An in-memory [`RecordFile`] standing in for `daemon.json` on disk.
#[derive(Default)]
pub struct InMemoryRecordFile {
    contents: RefCell<Option<String>>,
    read_calls: Cell<usize>,
    fail_read_on: Option<usize>,
    clear_on_read: Option<usize>,
    fail_remove: bool,
}

impl InMemoryRecordFile {
    /// A file pre-seeded with `contents`, as if a record were already persisted.
    pub fn with(contents: &str) -> Self {
        Self {
            contents: RefCell::new(Some(contents.to_string())),
            ..Self::default()
        }
    }

    /// A seeded file whose selected zero-based read call fails.
    pub fn failing_read_on(contents: &str, call: usize) -> Self {
        Self {
            contents: RefCell::new(Some(contents.to_string())),
            fail_read_on: Some(call),
            ..Self::default()
        }
    }

    /// A seeded file which disappears immediately before the selected read.
    pub fn clearing_on_read(contents: &str, call: usize) -> Self {
        Self {
            contents: RefCell::new(Some(contents.to_string())),
            clear_on_read: Some(call),
            ..Self::default()
        }
    }

    /// A seeded file whose conditional removal fails.
    pub fn failing_remove(contents: &str) -> Self {
        Self {
            contents: RefCell::new(Some(contents.to_string())),
            fail_remove: true,
            ..Self::default()
        }
    }
}

impl RecordFile for InMemoryRecordFile {
    fn read(&self) -> io::Result<Option<String>> {
        let call = self.read_calls.get();
        self.read_calls.set(call + 1);
        if self.fail_read_on == Some(call) {
            Err(io::Error::other("read failed"))
        } else {
            if self.clear_on_read == Some(call) {
                self.contents.borrow_mut().take();
            }
            Ok(self.contents.borrow().clone())
        }
    }

    fn write(&self, contents: &str) -> io::Result<()> {
        *self.contents.borrow_mut() = Some(contents.to_string());
        Ok(())
    }

    fn remove_if(&self, expected: &str) -> io::Result<bool> {
        if self.fail_remove {
            return Err(io::Error::other("remove failed"));
        }
        let mut contents = self.contents.borrow_mut();
        if contents.as_deref() == Some(expected) {
            *contents = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// A [`LivenessProbe`] that reports a fixed exact/gone outcome.
pub struct FixedProbe(pub bool);

impl LivenessProbe for FixedProbe {
    fn observe(&self, _record: &DaemonRecord) -> DaemonProcessObservation {
        if self.0 {
            DaemonProcessObservation::Exact
        } else {
            DaemonProcessObservation::Gone
        }
    }
}

impl ProcessIdentitySource for FixedProbe {
    fn process_start_identity(&self, pid: u32) -> io::Result<String> {
        Ok(format!("test:{pid}"))
    }
}

/// A [`LivenessProbe`] reporting one fixed observation, so lifecycle tests can
/// drive the PID-reuse and ownership-unknown arms the same way [`FixedProbe`]
/// drives exact / gone.
pub struct ObservedAs(pub DaemonProcessObservation);

impl LivenessProbe for ObservedAs {
    fn observe(&self, _record: &DaemonRecord) -> DaemonProcessObservation {
        self.0
    }
}

/// A [`Terminator`] that records the pids it is asked to terminate and can be
/// configured to fail, so tests can assert who was signalled and cover the
/// error path.
#[derive(Default)]
pub struct RecordingTerminator {
    fail: bool,
    terminated: RefCell<Vec<DaemonRecord>>,
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
        self.terminated
            .borrow()
            .iter()
            .map(|record| record.pid)
            .collect()
    }
}

impl Terminator for RecordingTerminator {
    fn terminate(&self, record: &DaemonRecord) -> io::Result<()> {
        self.terminated.borrow_mut().push(record.clone());
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
    fn prepare(&self) -> io::Result<()> {
        Ok(())
    }

    fn wait(&self) -> io::Result<()> {
        Ok(())
    }
}

/// A [`ShutdownSignal`] whose wait fails, to cover the error path.
pub struct FailingShutdown;

impl ShutdownSignal for FailingShutdown {
    fn prepare(&self) -> io::Result<()> {
        Ok(())
    }

    fn wait(&self) -> io::Result<()> {
        Err(io::Error::other("wait failed"))
    }
}

/// A [`DaemonReady`] that publishes nothing, for lifecycle tests that do not
/// exercise the endpoint boundary.
pub struct NoopReady;

impl DaemonReady for NoopReady {
    fn recover_stale_endpoint(&self) -> io::Result<()> {
        Ok(())
    }

    fn publish(&self) -> io::Result<()> {
        Ok(())
    }

    fn quiesce(&self) -> io::Result<()> {
        Ok(())
    }

    fn retire(&self) -> io::Result<()> {
        Ok(())
    }
}

/// A [`GenerationAuthority`] that records every claim and release, and can be
/// made to fail either one.
///
/// The counts are what the ordering assertions read: `serve` must claim after the
/// endpoint answers and release after it is retired, so a test proves the order
/// by what was recorded rather than by inspecting a registry.
#[derive(Default)]
pub struct FakeAuthority {
    claims: Cell<usize>,
    releases: Cell<usize>,
    fail_claim: bool,
    fail_release: bool,
}

impl FakeAuthority {
    /// An authority whose claim fails, as an unreadable registry would.
    pub fn failing_claim() -> Self {
        Self {
            fail_claim: true,
            ..Self::default()
        }
    }

    /// An authority whose release fails, as a registry that cannot be written
    /// during shutdown would.
    pub fn failing_release() -> Self {
        Self {
            fail_release: true,
            ..Self::default()
        }
    }

    /// How many times authority was claimed.
    pub fn claims(&self) -> usize {
        self.claims.get()
    }

    /// How many times authority was released.
    pub fn releases(&self) -> usize {
        self.releases.get()
    }
}

impl GenerationAuthority for FakeAuthority {
    fn claim(&self) -> io::Result<()> {
        self.claims.set(self.claims.get() + 1);
        if self.fail_claim {
            Err(io::Error::other("claim failed"))
        } else {
            Ok(())
        }
    }

    fn release(&self) -> io::Result<()> {
        self.releases.set(self.releases.get() + 1);
        if self.fail_release {
            Err(io::Error::other("release failed"))
        } else {
            Ok(())
        }
    }
}

impl StaleDaemonCleanup for NoopReady {
    fn cleanup_if(
        &self,
        store: &dyn DaemonRecordPort,
        expected: &DaemonRecord,
    ) -> io::Result<StaleCleanup> {
        match store.load()? {
            Some(current) if current == *expected && store.clear_if(expected)? => {
                Ok(StaleCleanup::Cleared)
            }
            Some(_) | None => Ok(StaleCleanup::Superseded),
        }
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
    launches: Cell<usize>,
}

impl<'a, F> TestLauncher<'a, F> {
    /// A launcher that registers `pid` into `store` on launch.
    pub fn registering(store: &'a DaemonRecordStore<F>, pid: u32) -> Self {
        Self {
            store,
            register_pid: Some(pid),
            launches: Cell::new(0),
        }
    }

    /// A launcher that spawns nothing, so no record ever appears.
    pub fn idle(store: &'a DaemonRecordStore<F>) -> Self {
        Self {
            store,
            register_pid: None,
            launches: Cell::new(0),
        }
    }

    /// How many detached daemons this launcher was asked to spawn.
    pub fn launches(&self) -> usize {
        self.launches.get()
    }
}

impl<F: RecordFile> DaemonLauncher for TestLauncher<'_, F> {
    fn launch(&self) -> io::Result<()> {
        self.launches.set(self.launches.get() + 1);
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

/// A [`WorkspaceFence`] with a fixed outcome, so `serve` tests exercise owning a
/// workspace, being refused by its current owner, and failing without touching a
/// real workspace.
pub enum FakeWorkspaceFence {
    /// This process now owns the workspace.
    Acquired,
    /// Another daemon owns the workspace and published its pid.
    Held(u32),
    /// Another daemon owns the workspace but no pid hint is readable.
    HeldAnonymously,
    /// Acquiring the fence fails.
    Failing,
}

impl WorkspaceFence for FakeWorkspaceFence {
    fn acquire(&self) -> io::Result<WorkspaceFenceOutcome> {
        match self {
            Self::Acquired => Ok(WorkspaceFenceOutcome::Acquired),
            Self::Held(owner) => Ok(WorkspaceFenceOutcome::Held {
                workspace: "/fixture/workspace".to_owned(),
                owner: Some(*owner),
            }),
            Self::HeldAnonymously => Ok(WorkspaceFenceOutcome::Held {
                workspace: "/fixture/workspace".to_owned(),
                owner: None,
            }),
            Self::Failing => Err(io::Error::other("workspace fence failed")),
        }
    }
}

//! In-memory doubles for the durable, platform, and clock seams.
//!
//! Two "processes" are two stores over the same [`SharedBytes`], which is exactly
//! what a cross-process compare-and-swap has to survive: both read the same bytes,
//! and only the first commit wins.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use usagi_core::domain::id::{
    DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
};

use crate::usecase::generation::ProcessIdentity;
use crate::usecase::resources::CasFile;
use crate::usecase::resources::allocator::{CapacityPolicy, ResourceAllocator, ResourceKind};
use crate::usecase::resources::durable::{IdentityAuthority, LegacySnapshots, ShardArchive};
use crate::usecase::resources::identity::{ChildIdentity, ChildProcessProbe, IDENTITY_SOURCE_OS};
use crate::usecase::resources::launch::{LaunchIntent, ResourceSpawner, SpawnRefusal};
use crate::usecase::resources::retention::{LogicalClock, RetentionLimits};
use crate::usecase::resources::shard::OwnerShard;

/// The bytes of one durable document, shared by every store bound to it.
#[derive(Clone, Default)]
pub struct SharedBytes(Arc<Mutex<Option<String>>>);

impl SharedBytes {
    /// The stored bytes, if the document exists.
    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }

    /// Overwrite the bytes, bypassing every compare-and-swap — used to inject
    /// corruption and foreign documents.
    pub fn set(&self, contents: &str) {
        *self.0.lock().unwrap() = Some(contents.to_owned());
    }
}

/// How a fake file misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFault {
    None,
    ReadFails,
    WriteFails,
    /// Every comparison fails, as if another writer always won the race.
    AlwaysStale,
}

/// A [`CasFile`] over shared memory.
pub struct MemoryFile {
    bytes: SharedBytes,
    fault: FileFault,
}

impl MemoryFile {
    /// A healthy file over `bytes`.
    pub fn new(bytes: &SharedBytes) -> Self {
        Self {
            bytes: bytes.clone(),
            fault: FileFault::None,
        }
    }

    /// A file that fails the given way.
    pub fn faulty(bytes: &SharedBytes, fault: FileFault) -> Self {
        Self {
            bytes: bytes.clone(),
            fault,
        }
    }
}

impl CasFile for MemoryFile {
    fn read(&self) -> io::Result<Option<String>> {
        if self.fault == FileFault::ReadFails {
            return Err(io::Error::other("read failed"));
        }
        Ok(self.bytes.get())
    }

    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool> {
        if self.fault == FileFault::WriteFails {
            return Err(io::Error::other("write failed"));
        }
        if self.fault == FileFault::AlwaysStale {
            return Ok(false);
        }
        let mut guard = self.bytes.0.lock().unwrap();
        if guard.as_deref() != expected {
            return Ok(false);
        }
        *guard = Some(contents.to_owned());
        Ok(true)
    }
}

/// A [`ShardArchive`] over shared memory.
///
/// Cloning it shares every document, so two "processes" can be bound to the same
/// archive exactly as two daemons share one data directory.
#[derive(Clone, Default)]
pub struct MemoryArchive {
    shards: Arc<Mutex<BTreeMap<String, SharedBytes>>>,
    legacy: Arc<Mutex<LegacySnapshots>>,
    marker: Arc<Mutex<Option<String>>>,
    collected: Arc<Mutex<Vec<String>>>,
}

impl MemoryArchive {
    /// An archive with no shard and nothing to migrate.
    pub fn new() -> Self {
        Self::default()
    }

    /// An archive holding the legacy whole-snapshot stores.
    pub fn with_legacy(agents: Option<&str>, terminals: Option<&str>) -> Self {
        let archive = Self::default();
        *archive.legacy.lock().unwrap() = LegacySnapshots {
            agents: agents.map(str::to_owned),
            terminals: terminals.map(str::to_owned),
        };
        archive
    }

    /// One generation's shard bytes, created empty on first use.
    pub fn bytes(&self, owner: DaemonGeneration) -> SharedBytes {
        self.shards
            .lock()
            .unwrap()
            .entry(owner.as_str())
            .or_default()
            .clone()
    }

    /// The migration marker, once the legacy stores were sealed.
    pub fn marker(&self) -> Option<String> {
        self.marker.lock().unwrap().clone()
    }

    /// The generations whose shard was collected, in collection order.
    pub fn collected(&self) -> Vec<String> {
        self.collected.lock().unwrap().clone()
    }
}

impl ShardArchive for MemoryArchive {
    fn documents(&self) -> io::Result<Vec<String>> {
        Ok(self
            .shards
            .lock()
            .unwrap()
            .values()
            .filter_map(SharedBytes::get)
            .collect())
    }

    fn shard(&self, owner: DaemonGeneration) -> io::Result<Box<dyn CasFile + Send>> {
        Ok(Box::new(MemoryFile::new(&self.bytes(owner))))
    }

    fn collect(&self, owner: DaemonGeneration) -> io::Result<()> {
        let name = owner.as_str();
        self.shards.lock().unwrap().remove(&name);
        self.collected.lock().unwrap().push(name);
        Ok(())
    }

    fn legacy(&self) -> io::Result<LegacySnapshots> {
        Ok(self.legacy.lock().unwrap().clone())
    }

    fn seal_legacy(&self, marker: &str) -> io::Result<()> {
        *self.marker.lock().unwrap() = Some(marker.to_owned());
        *self.legacy.lock().unwrap() = LegacySnapshots::default();
        Ok(())
    }
}

/// An identity authority that only vouches for the children it was told about.
#[derive(Debug, Default)]
pub struct ObservedChildren(BTreeMap<u32, String>);

impl ObservedChildren {
    /// An authority that proves nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that this process observed `pid` starting with `start`.
    pub fn with(mut self, pid: u32, start: &str) -> Self {
        self.0.insert(pid, start.to_owned());
        self
    }
}

impl IdentityAuthority for ObservedChildren {
    fn verified(&self, process: &ProcessIdentity) -> Option<ChildIdentity> {
        self.0
            .get(&process.pid)
            .filter(|start| *start == &process.start_identity)
            .map(|start| verified(process.pid, start))
    }
}

/// What the platform answers for one pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeAnswer {
    /// A live process with this start token and process group.
    Alive { start: String, group: u32 },
    /// No such process.
    Gone,
    /// The platform refused to answer.
    Denied,
    /// The platform answered with an unusable token.
    Malformed,
}

/// A [`ChildProcessProbe`] over a table of answers.
#[derive(Debug, Default)]
pub struct FakeProbe {
    answers: BTreeMap<u32, ProbeAnswer>,
}

impl FakeProbe {
    /// A probe that knows about nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what the platform answers for `pid`.
    pub fn with(mut self, pid: u32, answer: ProbeAnswer) -> Self {
        self.answers.insert(pid, answer);
        self
    }

    /// Replace one pid's answer, simulating exit and PID reuse.
    pub fn set(&mut self, pid: u32, answer: ProbeAnswer) {
        self.answers.insert(pid, answer);
    }
}

impl ChildProcessProbe for FakeProbe {
    fn start_identity(&self, pid: u32) -> io::Result<String> {
        match self.answers.get(&pid) {
            Some(ProbeAnswer::Alive { start, .. }) => Ok(start.clone()),
            Some(ProbeAnswer::Malformed) => Ok(String::new()),
            Some(ProbeAnswer::Denied) => Err(io::Error::other("permission denied")),
            _ => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    /// Only a live process has a group. Platform-specific group failures are
    /// spelled by dedicated probes in the identity tests, so this table stays one
    /// answer per pid.
    fn process_group(&self, pid: u32) -> io::Result<u32> {
        match self.answers.get(&pid) {
            Some(ProbeAnswer::Alive { group, .. }) => Ok(*group),
            _ => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }
}

/// A deterministic logical clock.
#[derive(Debug, Default)]
pub struct FakeClock(Cell<u64>);

impl FakeClock {
    /// A clock reading `now`.
    pub fn at(now: u64) -> Self {
        Self(Cell::new(now))
    }

    /// Move the clock forward.
    pub fn advance(&self, ticks: u64) {
        self.0.set(self.0.get() + ticks);
    }
}

impl LogicalClock for FakeClock {
    fn now(&self) -> u64 {
        self.0.get()
    }
}

/// What a fake spawn does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnPlan {
    /// Succeed with a child at this pid, whose identity the probe agrees with.
    Child { pid: u32, start: String },
    /// Fail definitely: no process exists.
    Definite,
    /// Succeed, but the identity could not be observed.
    Ambiguous,
}

/// A [`ResourceSpawner`] that counts every spawn, so "spawned at most once" is
/// checkable rather than assumed.
#[derive(Debug)]
pub struct FakeSpawner {
    plan: SpawnPlan,
    pub spawns: usize,
}

impl FakeSpawner {
    /// A spawner following `plan`.
    pub fn new(plan: SpawnPlan) -> Self {
        Self { plan, spawns: 0 }
    }
}

impl ResourceSpawner for FakeSpawner {
    fn spawn(&mut self, _resource: &TerminalRef) -> Result<ChildIdentity, SpawnRefusal> {
        self.spawns += 1;
        match &self.plan {
            SpawnPlan::Child { pid, start } => Ok(ChildIdentity {
                pid: *pid,
                process_group: *pid,
                source: IDENTITY_SOURCE_OS.to_owned(),
                start_identity: start.clone(),
            }),
            SpawnPlan::Definite => Err(SpawnRefusal::Definite),
            SpawnPlan::Ambiguous => Err(SpawnRefusal::Ambiguous),
        }
    }
}

/// An OS-verified identity, as a spawn would record it.
pub fn verified(pid: u32, start: &str) -> ChildIdentity {
    ChildIdentity {
        pid,
        process_group: pid,
        source: IDENTITY_SOURCE_OS.to_owned(),
        start_identity: start.to_owned(),
    }
}

/// A probe that agrees with [`verified`] for `pid`.
pub fn probe_for(pid: u32, start: &str) -> FakeProbe {
    FakeProbe::new().with(
        pid,
        ProbeAnswer::Alive {
            start: start.to_owned(),
            group: pid,
        },
    )
}

/// A fresh resource identity owned by `owner`.
pub fn terminal(owner: DaemonGeneration) -> TerminalRef {
    TerminalRef {
        daemon_generation: owner,
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    }
}

/// A canonical launch intent for `resource`.
pub fn intent(operation: &OperationId, digest: &str, resource: &TerminalRef) -> LaunchIntent {
    LaunchIntent {
        operation: *operation,
        digest: digest.to_owned(),
        kind: ResourceKind::Terminal,
        resource: resource.clone(),
    }
}

/// Generous limits, so a test that is not about retention never trips it.
pub fn wide_limits() -> RetentionLimits {
    RetentionLimits::new(64, 1 << 20, 1_000, 100, 200)
}

/// The two-pool policy used across the tests.
pub fn policy(agent: usize, terminal: usize) -> CapacityPolicy {
    CapacityPolicy::new(agent, terminal)
}

/// An allocator over `bytes`.
pub fn allocator(bytes: &SharedBytes, policy: CapacityPolicy) -> ResourceAllocator {
    ResourceAllocator::new(MemoryFile::new(bytes), policy)
}

/// A shard over `bytes` owned by `owner`.
pub fn shard(bytes: &SharedBytes, owner: DaemonGeneration) -> OwnerShard {
    OwnerShard::new(MemoryFile::new(bytes), owner)
}

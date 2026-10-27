//! Shipping terminal orchestration and owner-shard storage across real processes.
//!
//! Each worker uses `GenericTerminalCoordinator` and `ShardedTerminalStore`, so the
//! real PTY spawn is fenced by the same reservation and durable final as the
//! daemon. An old owner's exit and a successor's spawn must survive together;
//! restoring a producer operation must retain its identity without another PTY.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use usagi_core::domain::clock::LogicalClock;
use usagi_core::domain::id::{
    CompletionFence, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
    WorkspaceId, WorktreeId,
};
use usagi_core::domain::terminal_launch::{
    DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalLaunchRequest,
    TerminalLaunchScope, TerminalLaunchValidationError, TerminalProfileId, canonical_launch_digest,
};
use usagi_daemon::infrastructure::child_identity::UnixChildProbe;
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::infrastructure::resource_store::{
    AllocatorFile, OwnerShardFile, ShardArchiveFiles,
};
use usagi_daemon::usecase::generation::{GenerationRole, ProcessIdentity};
use usagi_daemon::usecase::generic_terminal::{
    GenericPtySpawner, GenericTerminalCoordinator, GenericTerminalError, TerminalProfileResolver,
};
use usagi_daemon::usecase::resources::allocator::{
    CapacityPolicy, ClaimState, OperationOutcome, ResourceAllocator, ResourceKind,
};
use usagi_daemon::usecase::resources::drain::ActiveConsumer;
use usagi_daemon::usecase::resources::durable::{
    IdentityAuthority, ShardedRuntimeState, ShardedTerminalStore,
};
use usagi_daemon::usecase::resources::identity::{
    ChildIdentity, ChildObservation, observe_child, record_child,
};
use usagi_daemon::usecase::resources::shard::{OwnerShard, ResourceState, collectable};
use usagi_daemon::usecase::terminal::{Geometry, PtyWriter, SpawnFailure};

static SERIAL: Mutex<()> = Mutex::new(());
const WORKER_ROLE: &str = "USAGI_RESOURCE_WORKER";
const WORKER_DATA_DIR: &str = "USAGI_RESOURCE_DATA_DIR";
const WORKER_GENERATION: &str = "USAGI_RESOURCE_GENERATION";
const WORKER_OPERATION: &str = "USAGI_RESOURCE_OPERATION";
const WORKER_TERMINAL: &str = "USAGI_RESOURCE_TERMINAL";
const DEADLINE: Duration = Duration::from_secs(20);
const GEOMETRY: Geometry = Geometry { cols: 80, rows: 24 };

struct FixedClock;
impl LogicalClock for FixedClock {
    fn now(&self) -> u64 {
        1
    }
}

struct OsIdentity;
impl IdentityAuthority for OsIdentity {
    fn verified(&self, process: &ProcessIdentity) -> Option<ChildIdentity> {
        let observed = record_child(&UnixChildProbe, process.pid).ok()?;
        (observed.start_identity == process.start_identity
            && observed.process_group == process.process_group)
            .then_some(observed)
    }

    fn observe(&self, identity: &ChildIdentity) -> ChildObservation {
        observe_child(&UnixChildProbe, identity)
    }
}

#[derive(Default)]
struct RealPtySpawner {
    children: Vec<(PtyTerminal, JoinHandle<()>)>,
    spawns: usize,
}

impl RealPtySpawner {
    fn end_children(&mut self) -> Vec<i32> {
        self.children
            .drain(..)
            .map(|(mut child, reader)| {
                child.write_all(b"exit\n").expect("PTY accepts exit");
                let status = child.wait().expect("child reports its exit status");
                drop(child);
                reader.join().expect("PTY reader finishes");
                status
            })
            .collect()
    }
}

impl Drop for RealPtySpawner {
    fn drop(&mut self) {
        // A failed assertion must still reap every child and stop its reader.
        for (child, reader) in self.children.drain(..) {
            let _ = child.terminate_reap();
            drop(child);
            let _ = reader.join();
        }
    }
}

impl GenericPtySpawner for RealPtySpawner {
    fn spawn(
        &mut self,
        launch: &ResolvedTerminalLaunch,
        _resource: &TerminalRef,
        geometry: Geometry,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        let terminal = PtyTerminal::spawn(
            &launch.snapshot.program,
            &launch.snapshot.working_directory,
            geometry,
        )
        .map_err(|_| SpawnFailure::Definite)?;
        let pid = terminal.process_id().ok_or(SpawnFailure::Ambiguous)?;
        let identity = record_child(&UnixChildProbe, pid).map_err(|_| SpawnFailure::Ambiguous)?;
        let mut reader = terminal.reader().map_err(|_| SpawnFailure::Ambiguous)?;
        let reader = std::thread::spawn(move || {
            let mut sink = [0u8; 4096];
            while reader.read(&mut sink).unwrap_or(0) > 0 {}
        });
        self.children.push((terminal, reader));
        self.spawns += 1;
        Ok(ProcessIdentity {
            pid,
            process_group: identity.process_group,
            start_identity: identity.start_identity,
        })
    }
}

struct ShellProfile(PathBuf);
impl TerminalProfileResolver for ShellProfile {
    fn resolve(
        &mut self,
        request: &TerminalLaunchRequest,
    ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
        ResolvedTerminalLaunch::new(
            DurableTerminalLaunchSnapshot::new(
                request.clone(),
                1,
                "/bin/sh",
                Vec::new(),
                self.0.clone(),
                [],
            )?,
            BTreeMap::new(),
        )
    }
}

fn allocator(data_dir: &Path) -> ResourceAllocator {
    ResourceAllocator::new(
        AllocatorFile::new(data_dir).unwrap(),
        CapacityPolicy::new(2, 4),
    )
}

fn shard(data_dir: &Path, generation: DaemonGeneration) -> OwnerShard {
    OwnerShard::new(
        OwnerShardFile::new(data_dir, generation).unwrap(),
        generation,
    )
}

fn shard_path(data_dir: &Path, generation: DaemonGeneration) -> PathBuf {
    data_dir
        .join("daemon/shards")
        .join(format!("{}.json", generation.as_str()))
}

fn terminal_of(generation: DaemonGeneration) -> TerminalRef {
    TerminalRef {
        daemon_generation: generation,
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    }
}

fn request(resource: &TerminalRef) -> TerminalLaunchRequest {
    TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").unwrap(),
        scope: TerminalLaunchScope {
            workspace_id: resource.workspace_id,
            session_id: resource.session_id,
            worktree_id: resource.worktree_id,
        },
    }
}

struct TerminalOwner {
    coordinator: GenericTerminalCoordinator,
    store: ShardedTerminalStore,
    spawner: RealPtySpawner,
    profile: ShellProfile,
}

impl TerminalOwner {
    fn restore(data_dir: &Path, generation: DaemonGeneration, role: GenerationRole) -> Self {
        let state = ShardedRuntimeState::new(
            generation,
            role,
            allocator(data_dir),
            Box::new(ShardArchiveFiles::new(data_dir).unwrap()),
            Box::new(OsIdentity),
            Box::new(FixedClock),
        )
        .unwrap();
        let snapshot = state.hydrate().unwrap().terminals;
        Self {
            coordinator: GenericTerminalCoordinator::from_snapshot(4, 4096, 16, snapshot).unwrap(),
            store: ShardedTerminalStore::new(state),
            spawner: RealPtySpawner::default(),
            profile: ShellProfile(data_dir.to_path_buf()),
        }
    }

    fn launch(
        &mut self,
        resource: &TerminalRef,
        operation: OperationId,
    ) -> Result<(), GenericTerminalError> {
        self.coordinator.launch(
            &request(resource),
            resource.clone(),
            CompletionFence {
                workspace_id: resource.workspace_id,
                session_id: resource.session_id,
                operation_id: operation,
                owner_daemon_generation: resource.daemon_generation,
                execution_attempt: 1,
                lifecycle_attempt: 1,
                expected_revision: 1,
            },
            GEOMETRY,
            &mut self.profile,
            &mut self.store,
            &mut self.spawner,
        )
    }

    fn assert_replay(&mut self, resource: &TerminalRef, operation: OperationId) {
        let record = self.coordinator.launch_by_operation(&operation).unwrap();
        assert_eq!(&record.terminal, resource);
        assert_eq!(
            record.launch_digest.as_deref(),
            Some(
                canonical_launch_digest(&request(resource), GEOMETRY.cols, GEOMETRY.rows).as_str()
            )
        );
        // The request-facing owner uses this record to replay; even an accidental
        // second call into the coordinator refuses before reaching the spawner.
        let spawns = self.spawner.spawns;
        assert_eq!(
            self.launch(resource, operation),
            Err(GenericTerminalError::TerminalAlreadyExists)
        );
        assert_eq!(self.spawner.spawns, spawns);
    }
}

#[test]
fn worker_process_entry_point() {
    let Ok(role) = std::env::var(WORKER_ROLE) else {
        return;
    };
    assert_eq!(role, "1");
    let data_dir = PathBuf::from(std::env::var(WORKER_DATA_DIR).unwrap());
    let generation = DaemonGeneration::parse(&std::env::var(WORKER_GENERATION).unwrap()).unwrap();
    let operation = OperationId::parse(&std::env::var(WORKER_OPERATION).unwrap()).unwrap();
    let resource: TerminalRef =
        serde_json::from_str(&std::env::var(WORKER_TERMINAL).unwrap()).unwrap();
    let mut owner = TerminalOwner::restore(&data_dir, generation, GenerationRole::Active);
    owner.launch(&resource, operation).unwrap();
    assert_eq!(owner.spawner.spawns, 1);
    owner.assert_replay(&resource, operation);
    assert_eq!(owner.spawner.end_children().len(), 1);
    // Leave the running durable record behind, as a process that disappeared
    // before persisting its observed exit would. A later hydrate must fence it.
}

fn spawn_worker(
    data_dir: &Path,
    generation: DaemonGeneration,
    operation: OperationId,
    resource: &TerminalRef,
) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "worker_process_entry_point", "--nocapture"])
        .env(WORKER_ROLE, "1")
        .env(WORKER_DATA_DIR, data_dir)
        .env(WORKER_GENERATION, generation.as_str())
        .env(WORKER_OPERATION, operation.as_str())
        .env(WORKER_TERMINAL, serde_json::to_string(resource).unwrap())
        .spawn()
        .unwrap()
}

fn await_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("{what} was not reached within {DEADLINE:?}");
}

#[test]
#[allow(clippy::too_many_lines)] // The ordering is the assertion in this two-process run.
fn an_old_exit_and_a_new_spawn_run_in_two_processes_without_losing_either() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = TempDir::new().unwrap();
    let data_dir = home.path();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let old_resource = terminal_of(old);
    let new_resource = terminal_of(new);
    let old_operation = OperationId::new();
    let new_operation = OperationId::new();
    let mut owner = TerminalOwner::restore(data_dir, old, GenerationRole::Draining);
    owner.launch(&old_resource, old_operation).unwrap();
    assert_eq!(owner.spawner.spawns, 1);
    let recorded = shard(data_dir, old)
        .load()
        .unwrap()
        .document()
        .resource(&old_resource)
        .unwrap()
        .process
        .clone()
        .unwrap();
    assert!(recorded.is_verifiable());
    assert_eq!(
        observe_child(&UnixChildProbe, &recorded),
        ChildObservation::Exact
    );

    let old_bytes = std::fs::read(shard_path(data_dir, old)).unwrap();
    let mut worker = spawn_worker(data_dir, new, new_operation, &new_resource);
    await_until("the new generation's shard", || {
        shard_path(data_dir, new).exists()
    });
    assert_eq!(
        std::fs::read(shard_path(data_dir, old)).unwrap(),
        old_bytes,
        "the successor's reservation leaves the old shard untouched"
    );
    let status = owner.spawner.end_children()[0];
    await_until("the draining owner's child being gone", || {
        observe_child(&UnixChildProbe, &recorded).is_definitely_gone()
    });
    owner
        .coordinator
        .exit(&old_resource, status, &mut owner.store)
        .unwrap();
    let published_bytes = std::fs::read(shard_path(data_dir, old)).unwrap();
    assert!(
        worker.wait().unwrap().success(),
        "the worker process failed"
    );
    assert_eq!(
        std::fs::read(shard_path(data_dir, old)).unwrap(),
        published_bytes,
        "the successor's final save leaves the published exit untouched"
    );

    let ledger = allocator(data_dir).load().unwrap().to_document();
    assert_eq!(
        ledger.operation(&new_operation).unwrap().outcome,
        OperationOutcome::Spawned
    );
    assert_eq!(ledger.claim(&old_resource).unwrap().state, ClaimState::Live);
    assert_eq!(ledger.pool_used(ResourceKind::Terminal), 2);

    let allocator = allocator(data_dir);
    let old_shard = shard(data_dir, old);
    let published = old_shard.load().unwrap().to_document();
    let before = std::fs::read(shard_path(data_dir, old)).unwrap();
    let report = ActiveConsumer::new(&allocator).consume(&published).unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(report.refused, 0);
    assert_eq!(
        std::fs::read(shard_path(data_dir, old)).unwrap(),
        before,
        "the active generation never writes the draining owner's shard"
    );
    let repeat = ActiveConsumer::new(&allocator).consume(&published).unwrap();
    assert_eq!(repeat.applied, 0);
    assert_eq!(repeat.duplicates, 1);
    let ledger = allocator.load().unwrap().to_document();
    assert_eq!(
        ledger.claim(&old_resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(ledger.pool_used(ResourceKind::Terminal), 1);
    assert_ne!(old_bytes, before, "the owner published into its own shard");
    let (reclaimed, _) = old_shard
        .update(|document| Ok(document.reclaim(&ledger)))
        .unwrap();
    assert_eq!(reclaimed, 1);
    let drained = old_shard.load().unwrap().to_document();
    assert_eq!(drained.unacked_outbox(), 0);
    assert_eq!(drained.live_resources(), 0);
    assert_eq!(collectable(&drained, &ledger), Ok(()));

    let new_shard = shard(data_dir, new).load().unwrap().to_document();
    assert_eq!(
        new_shard.resource(&new_resource).unwrap().state,
        ResourceState::Running
    );
    let new_child = new_shard
        .resource(&new_resource)
        .unwrap()
        .process
        .as_ref()
        .unwrap();
    assert!(new_child.is_verifiable());
    await_until("the other process's child being gone", || {
        observe_child(&UnixChildProbe, new_child).is_definitely_gone()
    });
}

#[test]
fn a_repeated_operation_replays_across_a_process_boundary_without_a_second_child() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = TempDir::new().unwrap();
    let data_dir = home.path();
    let generation = DaemonGeneration::new();
    let resource = terminal_of(generation);
    let operation = OperationId::new();
    let mut worker = spawn_worker(data_dir, generation, operation, &resource);
    assert!(worker.wait().unwrap().success());
    let ledger = allocator(data_dir).load().unwrap().to_document();
    let record = ledger.operation(&operation).unwrap();
    assert_eq!(record.outcome, OperationOutcome::Spawned);
    assert_eq!(record.resource, resource);
    let mut restored = TerminalOwner::restore(data_dir, generation, GenerationRole::Active);
    restored.assert_replay(&resource, operation);
    assert_eq!(restored.spawner.spawns, 0);
    let child = shard(data_dir, generation)
        .load()
        .unwrap()
        .document()
        .resource(&resource)
        .unwrap()
        .process
        .clone()
        .unwrap();
    await_until("the worker's child being gone", || {
        observe_child(&UnixChildProbe, &child).is_definitely_gone()
    });
}

#[test]
fn a_claim_without_payload_refuses_a_new_terminal_before_spawn() {
    let home = TempDir::new().unwrap();
    let data_dir = home.path();
    let old = DaemonGeneration::new();
    let next = DaemonGeneration::new();
    let original = terminal_of(old);
    let operation = OperationId::new();
    let digest = format!(
        "terminal:{}",
        canonical_launch_digest(&request(&original), GEOMETRY.cols, GEOMETRY.rows)
    );
    let allocator = allocator(data_dir);
    allocator
        .update(|document| {
            document.reserve(
                &operation,
                &digest,
                ResourceKind::Terminal,
                old,
                &original,
                allocator.policy(),
            )
        })
        .unwrap();
    let before = allocator.load().unwrap().to_document();
    let proposed = TerminalRef {
        daemon_generation: next,
        terminal_id: TerminalId::new(),
        ..original.clone()
    };
    let mut owner = TerminalOwner::restore(data_dir, next, GenerationRole::Active);

    assert_eq!(
        owner.launch(&proposed, operation),
        Err(GenericTerminalError::Store)
    );
    assert_eq!(owner.spawner.spawns, 0);
    assert_eq!(allocator.load().unwrap().to_document(), before);
    assert!(!shard_path(data_dir, next).exists());
}

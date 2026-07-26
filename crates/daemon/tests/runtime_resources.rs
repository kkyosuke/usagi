//! Owner shards and the global allocator against two real processes and real
//! PTY children.
//!
//! The state machine, the crash matrix, and the retention phases are covered by
//! unit tests over injected seams. What only two real processes can show is the
//! part the whole design exists for:
//!
//! * the old generation reaps a **real** PTY child and publishes its exit while
//!   the new generation spawns a **real** PTY child of its own, and both
//!   transitions survive in the shared allocator,
//! * each process writes only its own shard file, so the other's bytes are
//!   untouched even though both are running,
//! * a child's identity is the one the OS reports, so a record adopted after the
//!   child exits is `gone` rather than "still mine",
//! * a repeated producer operation replays across a process boundary without
//!   spawning a second child.
//!
//! The second process is this test binary re-executed in a worker role, so the
//! two writers are genuinely separate address spaces.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use usagi_core::domain::id::{
    DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
};
use usagi_daemon::infrastructure::child_identity::UnixChildProbe;
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::infrastructure::resource_store::{AllocatorFile, OwnerShardFile};
use usagi_daemon::usecase::resources::allocator::{
    CapacityPolicy, ClaimState, OperationOutcome, ResourceAllocator, ResourceKind,
};
use usagi_daemon::usecase::resources::drain::{ActiveConsumer, publish_exit, reclaim_outbox};
use usagi_daemon::usecase::resources::identity::{
    ChildIdentity, ChildObservation, observe_child, record_child,
};
use usagi_daemon::usecase::resources::launch::{
    LaunchIntent, ResourceSpawner, SpawnRefusal, execute_launch,
};
use usagi_daemon::usecase::resources::retention::{LogicalClock, RetentionLimits};
use usagi_daemon::usecase::resources::shard::{OwnerShard, ResourceState, collectable};
use usagi_daemon::usecase::terminal::Geometry;

const WORKER_ROLE: &str = "USAGI_RESOURCE_WORKER";
const WORKER_DATA_DIR: &str = "USAGI_RESOURCE_DATA_DIR";
const WORKER_GENERATION: &str = "USAGI_RESOURCE_GENERATION";
const WORKER_OPERATION: &str = "USAGI_RESOURCE_OPERATION";
const WORKER_TERMINAL: &str = "USAGI_RESOURCE_TERMINAL";
const WORKER_DIGEST: &str = "test-digest";
const DEADLINE: Duration = Duration::from_secs(20);

/// A logical clock that never moves, so retention plays no part in these runs.
struct FixedClock;
impl LogicalClock for FixedClock {
    fn now(&self) -> u64 {
        1
    }
}

/// Spawns a real login-shell child under a real PTY and records the identity the
/// OS reports for it.
struct RealPtySpawner {
    directory: PathBuf,
    children: Vec<PtyTerminal>,
}

impl RealPtySpawner {
    fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_path_buf(),
            children: Vec::new(),
        }
    }
}

impl ResourceSpawner for RealPtySpawner {
    fn spawn(&mut self, _resource: &TerminalRef) -> Result<ChildIdentity, SpawnRefusal> {
        let terminal =
            PtyTerminal::spawn("/bin/sh", &self.directory, Geometry { cols: 80, rows: 24 })
                .map_err(|_| SpawnRefusal::Definite)?;
        let pid = terminal.process_id().ok_or(SpawnRefusal::Ambiguous)?;
        let identity = record_child(&UnixChildProbe, pid).map_err(|_| SpawnRefusal::Ambiguous)?;
        // Keeping the master alive keeps the child alive, which is what makes the
        // later observation a real "still running" answer.
        self.children.push(terminal);
        Ok(identity)
    }
}

fn allocator(data_dir: &Path) -> ResourceAllocator<AllocatorFile> {
    ResourceAllocator::new(
        AllocatorFile::new(data_dir).unwrap(),
        CapacityPolicy::new(2, 4),
    )
}

fn shard(data_dir: &Path, generation: DaemonGeneration) -> OwnerShard<OwnerShardFile> {
    OwnerShard::new(
        OwnerShardFile::new(data_dir, generation).unwrap(),
        generation,
    )
}

fn limits() -> RetentionLimits {
    RetentionLimits::new(64, 1 << 20, 10_000, 1_000, 2_000)
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

/// Runs one launch and hands back the spawner, because dropping it closes the
/// PTY master and the real child exits with it.
fn launch(
    data_dir: &Path,
    generation: DaemonGeneration,
    operation: OperationId,
    resource: &TerminalRef,
    directory: &Path,
) -> (
    usagi_daemon::usecase::resources::launch::LaunchAccepted,
    RealPtySpawner,
) {
    let mut spawner = RealPtySpawner::new(directory);
    let accepted = execute_launch(
        &allocator(data_dir),
        &shard(data_dir, generation),
        &LaunchIntent {
            operation,
            digest: WORKER_DIGEST.to_owned(),
            kind: ResourceKind::Terminal,
            resource: resource.clone(),
        },
        &mut spawner,
        &UnixChildProbe,
        &FixedClock,
        &limits(),
    )
    .expect("a launch against a healthy store is accepted");
    (accepted, spawner)
}

/// The second process: it launches one real PTY child for the operation it is
/// given, into the shard it owns.
#[test]
fn worker_process_entry_point() {
    let Ok(role) = std::env::var(WORKER_ROLE) else {
        // Ordinary `cargo test` run: this entry point is not the worker.
        return;
    };
    assert_eq!(role, "1");
    let data_dir = PathBuf::from(std::env::var(WORKER_DATA_DIR).unwrap());
    let generation = DaemonGeneration::parse(&std::env::var(WORKER_GENERATION).unwrap()).unwrap();
    let operation = OperationId::parse(&std::env::var(WORKER_OPERATION).unwrap()).unwrap();
    let resource: TerminalRef =
        serde_json::from_str(&std::env::var(WORKER_TERMINAL).unwrap()).unwrap();
    let directory = data_dir.join("worker-cwd");
    std::fs::create_dir_all(&directory).unwrap();

    let (accepted, held) = launch(&data_dir, generation, operation, &resource, &directory);
    assert_eq!(accepted.outcome, OperationOutcome::Spawned);
    assert_eq!(held.children.len(), 1);
    // Re-running the identical operation in the same process must not spawn a
    // second child either.
    let (replay, again) = launch(&data_dir, generation, operation, &resource, &directory);
    assert_eq!(replay.resource, accepted.resource);
    assert_eq!(replay.revision, accepted.revision);
    assert!(again.children.is_empty());
    drop(held);
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

fn await_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition was not reached within {DEADLINE:?}");
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end run: splitting it would hide the ordering.
fn an_old_exit_and_a_new_spawn_run_in_two_processes_without_losing_either() {
    let home = TempDir::new_in("/tmp").unwrap();
    let data_dir = home.path();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let old_resource = terminal_of(old);
    let new_resource = terminal_of(new);
    let old_operation = OperationId::new();
    let new_operation = OperationId::new();

    // The draining generation owns a real PTY child in its own shard.
    let (accepted, held) = launch(data_dir, old, old_operation, &old_resource, data_dir);
    assert_eq!(accepted.outcome, OperationOutcome::Spawned);
    assert_eq!(held.children.len(), 1);
    let recorded = shard(data_dir, old)
        .load()
        .unwrap()
        .to_document()
        .resource(&old_resource)
        .unwrap()
        .process
        .clone()
        .expect("a running record carries its child identity");
    assert!(recorded.is_verifiable());
    assert_eq!(
        observe_child(&UnixChildProbe, &recorded),
        ChildObservation::Exact,
        "the OS confirms the exact child this owner spawned"
    );

    // The new generation runs in a genuinely separate process, and it spawns
    // while the old shard still holds a live resource.
    let mut worker = spawn_worker(data_dir, new, new_operation, &new_resource);
    let new_shard_path = data_dir
        .join("daemon")
        .join("shards")
        .join(format!("{}.json", new.as_str()));
    await_until(|| new_shard_path.exists());
    let old_bytes = std::fs::read(
        data_dir
            .join("daemon")
            .join("shards")
            .join(format!("{}.json", old.as_str())),
    )
    .unwrap();

    // Meanwhile the draining owner reaps its child and publishes the exit.
    let child = shard(data_dir, old)
        .load()
        .unwrap()
        .to_document()
        .resource(&old_resource)
        .unwrap()
        .clone();
    let pid = child.process.as_ref().unwrap().pid;
    // SAFETY: SIGKILL to the exact pid this test spawned under its own PTY.
    unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), libc::SIGKILL) };
    await_until(|| {
        observe_child(&UnixChildProbe, child.process.as_ref().unwrap()).is_definitely_gone()
    });
    publish_exit(&shard(data_dir, old), &old_resource, 137).unwrap();

    assert!(
        worker.wait().unwrap().success(),
        "the worker process failed"
    );

    // Both transitions are durable: the new owner's claim and the old owner's
    // published exit. A whole-snapshot store would have lost one of them.
    let ledger = allocator(data_dir).load().unwrap().to_document();
    assert_eq!(
        ledger
            .operation(&new_operation)
            .map(|record| record.outcome),
        Some(OperationOutcome::Spawned),
        "the new process's spawn survived"
    );
    assert_eq!(
        ledger.claim(&old_resource).unwrap().state,
        ClaimState::Live,
        "the exit is published but not consumed yet"
    );
    assert_eq!(ledger.pool_used(ResourceKind::Terminal), 2);

    // Each process wrote only its own shard.
    assert!(
        !std::fs::read(
            data_dir
                .join("daemon")
                .join("shards")
                .join(format!("{}.json", old.as_str())),
        )
        .unwrap()
        .is_empty()
    );
    assert!(new_shard_path.exists());

    // The active generation consumes the old owner's outbox: capacity is
    // released exactly once and the old shard is never written by the consumer.
    let allocator = allocator(data_dir);
    let old_shard = shard(data_dir, old);
    let published = old_shard.load().unwrap().to_document();
    let before = std::fs::read(
        data_dir
            .join("daemon")
            .join("shards")
            .join(format!("{}.json", old.as_str())),
    )
    .unwrap();
    let report = ActiveConsumer::new(&allocator).consume(&published).unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(report.refused, 0);
    assert_eq!(
        std::fs::read(
            data_dir
                .join("daemon")
                .join("shards")
                .join(format!("{}.json", old.as_str())),
        )
        .unwrap(),
        before,
        "the active generation never writes the draining owner's shard"
    );
    // A redelivered pass changes nothing.
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

    // The owner reclaims its outbox and then, and only then, becomes collectable.
    assert_eq!(reclaim_outbox(&old_shard, &allocator).unwrap(), 1);
    let drained = old_shard.load().unwrap().to_document();
    assert_eq!(drained.unacked_outbox(), 0);
    assert_eq!(drained.live_resources(), 0);
    assert_eq!(collectable(&drained, &ledger), Ok(()));

    // The new generation's own shard still holds its running child.
    let new_shard = shard(data_dir, new).load().unwrap().to_document();
    assert_eq!(
        new_shard.resource(&new_resource).unwrap().state,
        ResourceState::Running
    );
    let new_child = new_shard
        .resource(&new_resource)
        .unwrap()
        .process
        .clone()
        .unwrap();
    assert!(new_child.is_verifiable());
    // The worker process exited, taking its PTY master with it. The identity is
    // still exact enough to answer the only question that matters: this child is
    // definitely gone, so nothing signals or adopts its pid.
    await_until(|| observe_child(&UnixChildProbe, &new_child).is_definitely_gone());
    drop(held);
}

#[test]
fn a_repeated_operation_replays_across_a_process_boundary_without_a_second_child() {
    let home = TempDir::new_in("/tmp").unwrap();
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

    // This process retries the same producer operation. It must replay, not
    // spawn: the child belongs to the process that is already gone, and its
    // identity says so.
    let (replay, spawner) = launch(data_dir, generation, operation, &resource, data_dir);
    assert!(spawner.children.is_empty(), "a replay spawns nothing");
    assert_eq!(replay.resource, resource);
    assert_eq!(replay.outcome, OperationOutcome::Spawned);
    assert!(!replay.spawned);

    let child = shard(data_dir, generation)
        .load()
        .unwrap()
        .to_document()
        .resource(&resource)
        .unwrap()
        .process
        .clone()
        .unwrap();
    // The worker exited, so its PTY master closed and the shell was reaped: the
    // OS answer is "gone" (or "reused"), never "this is still my child".
    await_until(|| observe_child(&UnixChildProbe, &child).is_definitely_gone());
}

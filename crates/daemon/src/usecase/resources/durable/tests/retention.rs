//! Admission limits exercised through the shipping store, including its CAS.

use super::*;
use crate::usecase::resources::retention::serialized_bytes;
use crate::usecase::resources::{CasFile, StoreError};

fn limited(world: &World, owner: DaemonGeneration, count: usize) -> ShardedRuntimeState {
    let mut state = world.state(owner, ObservedChildren::new());
    state.retention_limits.max_operations = count;
    state
}

fn failed_operation(document: &mut AllocatorDocument, owner: DaemonGeneration) -> OperationId {
    let operation = OperationId::new();
    document
        .reserve(
            &operation,
            "old",
            ResourceKind::Terminal,
            owner,
            &terminal(owner),
            policy(2, 2),
        )
        .unwrap();
    document
        .mark_failed(&operation, LaunchFailure::Spawn, 1)
        .unwrap();
    operation
}

#[test]
fn the_shipping_default_refuses_the_4097th_protected_operation() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let mut document = AllocatorDocument::default();
    for _ in 0..4096 {
        failed_operation(&mut document, owner);
    }
    assert!(serialized_bytes(&document) < shipping_retention_limits().max_bytes);
    world
        .allocator
        .set(&serde_json::to_string(&document).unwrap());
    let before = world.allocator.get();
    let mut store = ShardedTerminalStore::new(world.state(owner, ObservedChildren::new()));
    let record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    assert_eq!(
        store.save(terminal_snapshot(vec![record])),
        Err(StoreError::RetentionBackpressure)
    );
    assert_eq!(world.allocator.get(), before);
    assert!(world.archive.bytes(owner).get().is_none());
}

#[test]
fn shipping_stores_refuse_fresh_count_pressure_without_writing_either_document() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let mut document = AllocatorDocument::default();
    let protected = failed_operation(&mut document, owner);
    world
        .allocator
        .set(&serde_json::to_string(&document).unwrap());
    let before = world.allocator.get();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let mut terminals = ShardedTerminalStore::new(limited(&world, owner, 1));
    assert_eq!(
        terminals.save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Reserved,
            None
        )])),
        Err(StoreError::RetentionBackpressure)
    );
    let mut agents = ShardedAgentStore::new(limited(&world, owner, 1));
    assert_eq!(
        agents.save(agent_snapshot(vec![agent_record(
            &resource,
            operation,
            RuntimeState::Reserved,
            None
        )])),
        Err(StoreError::RetentionBackpressure)
    );
    assert_eq!(world.allocator.get(), before);
    assert!(world.archive.bytes(owner).get().is_none());
    assert_eq!(
        world.allocator().operation(&protected).unwrap().outcome,
        OperationOutcome::Failed(LaunchFailure::Spawn)
    );
}

#[test]
fn byte_limit_includes_the_new_reservation_and_revision_digit_growth() {
    let owner = DaemonGeneration::new();
    let record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    let initial = AllocatorDocument {
        revision: 9,
        ..AllocatorDocument::default()
    };
    let probe = World::new();
    probe
        .allocator
        .set(&serde_json::to_string(&initial).unwrap());
    ShardedTerminalStore::new(probe.state(owner, ObservedChildren::new()))
        .save(terminal_snapshot(vec![record.clone()]))
        .unwrap();
    let required = serialized_bytes(&probe.allocator());
    assert_eq!(probe.allocator().revision, 10);

    let world = World::new();
    world
        .allocator
        .set(&serde_json::to_string(&initial).unwrap());
    let before = world.allocator.get();
    let mut state = world.state(owner, ObservedChildren::new());
    state.retention_limits.max_bytes = required - 1;
    let mut store = ShardedTerminalStore::new(state);
    assert_eq!(
        store.save(terminal_snapshot(vec![record.clone()])),
        Err(StoreError::RetentionBackpressure)
    );
    assert_eq!(world.allocator.get(), before);
    assert!(world.archive.bytes(owner).get().is_none());
    store.state.retention_limits.max_bytes = required;
    store.save(terminal_snapshot(vec![record])).unwrap();
    assert_eq!(serialized_bytes(&world.allocator()), required);
}

#[test]
fn pressure_never_blocks_finalization_or_exact_replay_of_a_released_operation() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let mut record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    let mut store = ShardedTerminalStore::new(limited(&world, owner, 1));
    store.save(terminal_snapshot(vec![record.clone()])).unwrap();
    // Already admitted work must finish even if older data exceeds the cap.
    store.state.retention_limits.max_bytes = 1;
    record.state = TerminalRuntimeState::SpawnFailed;
    store.save(terminal_snapshot(vec![record.clone()])).unwrap();
    store.save(terminal_snapshot(vec![record.clone()])).unwrap();
    assert_eq!(
        world.allocator().claim(&record.terminal).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(
        world
            .allocator()
            .operation(&record.operation.operation_id)
            .unwrap()
            .outcome,
        OperationOutcome::Failed(LaunchFailure::Spawn)
    );
    let before = world.allocator.get();
    record.state = TerminalRuntimeState::Reserved;
    record.terminal = terminal(owner);
    assert_eq!(
        store.save(terminal_snapshot(vec![record])),
        Err(StoreError::Unavailable)
    );
    assert_eq!(
        world.allocator.get(),
        before,
        "pressure must not weaken exact-resource fencing"
    );
}

#[test]
fn admission_collects_only_safe_history_in_the_same_allocator_commit() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let mut document = AllocatorDocument::default();
    let old = failed_operation(&mut document, owner);
    world
        .allocator
        .set(&serde_json::to_string(&document).unwrap());
    let mut state = limited(&world, owner, 1);
    state.retention_limits = RetentionLimits::new(1, usize::MAX, 0, 0, 0);
    let mut store = ShardedTerminalStore::new(state);
    let record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    store.save(terminal_snapshot(vec![record.clone()])).unwrap();
    let after = world.allocator();
    assert_eq!(after.revision, 1, "collection and admission share one CAS");
    assert!(after.operation(&old).is_none());
    assert!(after.tombstones.is_empty());
    assert_eq!(after.watermark, Some(old));
    assert!(after.operation(&record.operation.operation_id).is_some());
    assert_eq!(
        after.admit(&old, "old", ResourceKind::Terminal, policy(2, 2)),
        Err(ResourceError::OperationExpired)
    );
}

#[test]
fn insufficient_collection_does_not_publish_tombstones_or_shorten_replay() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let mut document = AllocatorDocument::default();
    failed_operation(&mut document, owner);
    world
        .allocator
        .set(&serde_json::to_string(&document).unwrap());
    let before = world.allocator.get();
    let mut state = limited(&world, owner, 1);
    state.retention_limits.min_window = 0; // G1 is safe, but G3 is still protected.
    let mut store = ShardedTerminalStore::new(state);
    let record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    assert_eq!(
        store.save(terminal_snapshot(vec![record])),
        Err(StoreError::RetentionBackpressure)
    );
    assert_eq!(world.allocator.get(), before);
    assert!(world.archive.bytes(owner).get().is_none());
}

#[test]
fn shard_write_retry_keeps_its_existing_claim_under_pressure() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    world.archive.bytes(owner).set("invalid shard");
    let mut store = ShardedTerminalStore::new(limited(&world, owner, 1));
    let record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    assert_eq!(
        store.save(terminal_snapshot(vec![record.clone()])),
        Err(StoreError::Unavailable)
    );
    assert!(
        world
            .allocator()
            .operation(&record.operation.operation_id)
            .is_some()
    );
    world
        .archive
        .bytes(owner)
        .set(&serde_json::to_string(&ShardDocument::empty(owner)).unwrap());
    store.state.retention_limits.max_bytes = 1;
    store.save(terminal_snapshot(vec![record.clone()])).unwrap();
    assert_eq!(world.allocator().operations.len(), 1);
    assert!(world.shard(owner).resource(&record.terminal).is_some());
}

#[test]
fn a_competing_reservation_rechecks_retention_after_stale_cas() {
    struct RacingFile {
        bytes: SharedBytes,
        rival: std::sync::Mutex<Option<String>>,
    }
    impl CasFile for RacingFile {
        fn read(&self) -> std::io::Result<Option<String>> {
            Ok(self.bytes.get())
        }
        fn compare_and_write(
            &self,
            expected: Option<&str>,
            contents: &str,
        ) -> std::io::Result<bool> {
            if let Some(rival) = self.rival.lock().unwrap().take() {
                self.bytes.set(&rival);
                return Ok(false);
            }
            MemoryFile::new(&self.bytes).compare_and_write(expected, contents)
        }
    }
    let world = World::new();
    let owner = DaemonGeneration::new();
    let mut rival = AllocatorDocument::default();
    let rival_operation = OperationId::new();
    rival
        .reserve(
            &rival_operation,
            "rival",
            ResourceKind::Terminal,
            owner,
            &terminal(owner),
            policy(2, 2),
        )
        .unwrap();
    rival.revision = 1;
    let rival_bytes = serde_json::to_string(&rival).unwrap();
    let mut state = limited(&world, owner, 1);
    state.allocator = ResourceAllocator::new(
        RacingFile {
            bytes: world.allocator.clone(),
            rival: std::sync::Mutex::new(Some(rival_bytes.clone())),
        },
        policy(2, 2),
    );
    let mut store = ShardedTerminalStore::new(state);
    let record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    assert_eq!(
        store.save(terminal_snapshot(vec![record])),
        Err(StoreError::RetentionBackpressure)
    );
    assert_eq!(world.allocator.get().as_deref(), Some(rival_bytes.as_str()));
    assert!(world.archive.bytes(owner).get().is_none());
}

#[test]
fn coordinator_rolls_back_an_effect_zero_shipping_refusal_before_spawn() {
    use crate::usecase::generic_terminal::{
        GenericPtySpawner, GenericTerminalCoordinator, GenericTerminalError,
        TerminalProfileResolver,
    };
    use crate::usecase::terminal::{Geometry, SpawnFailure};
    use usagi_core::domain::terminal_launch::{
        ResolvedTerminalLaunch, TerminalLaunchValidationError,
    };

    struct Resolver(DurableTerminalLaunchSnapshot);
    impl TerminalProfileResolver for Resolver {
        fn resolve(
            &mut self,
            _: &TerminalLaunchRequest,
        ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
            ResolvedTerminalLaunch::new(self.0.clone(), BTreeMap::default())
        }
    }
    struct NoSpawn;
    impl GenericPtySpawner for NoSpawn {
        fn spawn(
            &mut self,
            _: &ResolvedTerminalLaunch,
            _: &TerminalRef,
            _: Geometry,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            panic!("refused admission must not spawn")
        }
    }
    struct Spawner(usize);
    impl GenericPtySpawner for Spawner {
        fn spawn(
            &mut self,
            _: &ResolvedTerminalLaunch,
            _: &TerminalRef,
            _: Geometry,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            self.0 += 1;
            Ok(process(7, "start-7"))
        }
    }
    let world = World::new();
    let owner = DaemonGeneration::new();
    let record = terminal_record(
        &terminal(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    let mut coordinator = GenericTerminalCoordinator::new(2, 64, 1);
    let mut store = ShardedTerminalStore::new(limited(&world, owner, 0));
    for _ in 0..2 {
        assert_eq!(
            coordinator.launch(
                &record.launch.request,
                record.terminal.clone(),
                record.operation.clone(),
                Geometry { cols: 80, rows: 24 },
                &mut Resolver(record.launch.clone()),
                &mut store,
                &mut NoSpawn
            ),
            Err(GenericTerminalError::RetentionBackpressure)
        );
        assert!(coordinator.snapshot().records.is_empty());
        assert_eq!(coordinator.retention().metrics().reserved_finals, 0);
        assert!(world.allocator.get().is_none());
        assert!(world.archive.bytes(owner).get().is_none());
    }
    store.state.retention_limits.max_operations = 1;
    store.state.identity = Box::new(ObservedChildren::new().with(7, "start-7"));
    let mut spawner = Spawner(0);
    coordinator
        .launch(
            &record.launch.request,
            record.terminal.clone(),
            record.operation,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver(record.launch.clone()),
            &mut store,
            &mut spawner,
        )
        .unwrap();
    assert_eq!(spawner.0, 1);
    assert_eq!(
        coordinator.snapshot().records[0].state,
        TerminalRuntimeState::Running
    );
    assert_eq!(
        world.allocator().claim(&record.terminal).unwrap().state,
        ClaimState::Live
    );
}

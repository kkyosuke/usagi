//! The shipping durable runtime state against a real data directory.
//!
//! The projection, the migration contract, and the crash matrix are covered by
//! unit tests over injected seams. What only real files can show is the part the
//! filesystem adapter owns:
//!
//! * each generation writes its own shard path, and the two documents are separate
//!   objects a draining owner and a new active owner never share,
//! * the legacy whole-snapshot stores are adopted from the bytes an older build
//!   left — at that build's file mode — and are then retired by rename, which is
//!   what makes the migration one way,
//! * the marker survives, so a data directory can say what happened to it,
//! * a fully drained generation's shard is removed, and a retained one is not,
//! * a census reads all of it without reconciling, migrating, or collecting.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use usagi_core::domain::agent::{
    AgentProfileId, DurableLaunchSnapshot, LaunchMode, LaunchPlan, LaunchRequest, LaunchScope,
};
use usagi_core::domain::id::{AgentRuntimeId, AgentRuntimeRef};
use usagi_core::domain::id::{
    CompletionFence, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
    WorkspaceId, WorktreeId,
};
use usagi_core::domain::terminal_launch::{
    DurableTerminalLaunchSnapshot, TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_daemon::infrastructure::resource_store::{AllocatorFile, ShardArchiveFiles};
use usagi_daemon::usecase::generation::GenerationRole;
use usagi_daemon::usecase::generic_terminal::{
    DurableTerminalRecord, TerminalStore, TerminalStoreSnapshot,
};
use usagi_daemon::usecase::resources::allocator::{CapacityPolicy, ResourceAllocator};
use usagi_daemon::usecase::resources::durable::{
    IdentityAuthority, MIGRATION_SCHEMA, ShardedAgentStore, ShardedRuntimeState,
    ShardedTerminalStore, UnprovenChildren, census, shipping_retention_limits,
};
use usagi_daemon::usecase::resources::retention::LogicalClock;
use usagi_daemon::usecase::runtime::{
    DurableOperationOutcome, DurableRuntimeRecord, RuntimeStoreSnapshot,
};
use usagi_daemon::usecase::terminal::{TerminalReconcileState, TerminalRuntimeState};

/// A clock a test can read but never has to wait for.
struct FixedClock(u64);

impl LogicalClock for FixedClock {
    fn now(&self) -> u64 {
        self.0
    }
}

fn state(data_dir: &Path, owner: DaemonGeneration) -> ShardedRuntimeState {
    ShardedRuntimeState::new(
        owner,
        GenerationRole::Active,
        ResourceAllocator::new(
            AllocatorFile::new(data_dir).unwrap(),
            CapacityPolicy::new(4, 4),
        ),
        Box::new(ShardArchiveFiles::new(data_dir).unwrap()),
        Box::new(UnprovenChildren),
        Box::new(FixedClock(100)),
    )
    .unwrap()
}

fn shard_path(data_dir: &Path, owner: DaemonGeneration) -> PathBuf {
    data_dir
        .join("daemon")
        .join("shards")
        .join(format!("{}.json", owner.as_str()))
}

fn record(owner: DaemonGeneration, state: TerminalRuntimeState) -> DurableTerminalRecord {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let worktree = WorktreeId::new();
    let scope = TerminalLaunchScope {
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: worktree,
    };
    DurableTerminalRecord {
        terminal: TerminalRef {
            daemon_generation: owner,
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        },
        operation: CompletionFence {
            workspace_id: workspace,
            session_id: Some(session),
            operation_id: OperationId::new(),
            owner_daemon_generation: owner,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 1,
        },
        launch: DurableTerminalLaunchSnapshot::new(
            TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell").unwrap(),
                scope,
            },
            1,
            "sh",
            Vec::new(),
            PathBuf::from("/tmp"),
            [],
        )
        .unwrap(),
        state,
        process: None,
        launch_digest: Some("digest".to_owned()),
    }
}

fn snapshot(records: Vec<DurableTerminalRecord>) -> TerminalStoreSnapshot {
    TerminalStoreSnapshot {
        records,
        ..TerminalStoreSnapshot::default()
    }
}

/// One legacy Agent record, as an older build wrote it: a running runtime whose
/// recorded identity is the fixed token that proves nothing.
fn agent_record(owner: DaemonGeneration) -> DurableRuntimeRecord {
    let terminal = record(owner, TerminalRuntimeState::Running);
    let profile = AgentProfileId::new("codex").unwrap();
    let scope = LaunchScope {
        workspace_id: terminal.terminal.workspace_id,
        session_id: terminal.terminal.session_id,
        worktree_id: terminal.terminal.worktree_id,
    };
    DurableRuntimeRecord {
        runtime: AgentRuntimeRef {
            agent_runtime_id: AgentRuntimeId::new(),
            terminal: terminal.terminal.clone(),
            session_id: terminal.terminal.session_id,
        },
        operation: terminal.operation,
        launch: DurableLaunchSnapshot::new(
            LaunchRequest {
                profile_id: profile.clone(),
                mode: LaunchMode::Interactive,
                model: None,
                resume: false,
                provider_resume: None,
                initial_prompt: None,
                scope,
                required_capabilities: std::collections::BTreeSet::new(),
            },
            LaunchPlan::new(profile, 1, "codex", Vec::new(), [], PathBuf::from("/tmp")).unwrap(),
        ),
        state: TerminalRuntimeState::Running,
        process: Some(usagi_daemon::usecase::generation::ProcessIdentity {
            pid: 4321,
            start_identity: "daemon-owned-agent-pty".to_owned(),
            process_group: 4321,
        }),
        provider_resume: None,
        continuation: None,
        resume_source: None,
        resumed_from: None,
        superseded_by: None,
        semantic_key: Some("intent".to_owned()),
        outcome: DurableOperationOutcome::Accepted,
        credential_provenance: None,
    }
}

/// A legacy store as an older build left it: inside the private daemon directory,
/// at that build's own file mode.
fn write_legacy(data_dir: &Path, name: &str, contents: &str) {
    let archive = ShardArchiveFiles::new(data_dir).unwrap();
    drop(archive);
    fs::write(data_dir.join("daemon").join(name), contents).unwrap();
}

#[test]
fn two_generations_write_two_separate_shard_files() {
    let dir = TempDir::new().unwrap();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let old_record = record(old, TerminalRuntimeState::Reserved);
    let new_record = record(new, TerminalRuntimeState::Reserved);

    let mut old_store = ShardedTerminalStore::new(state(dir.path(), old));
    assert_eq!(old_store.state().owner(), old);
    old_store.save(snapshot(vec![old_record.clone()])).unwrap();
    ShardedTerminalStore::new(state(dir.path(), new))
        .save(snapshot(vec![new_record.clone()]))
        .unwrap();

    // Two documents, one per owner: no write of either could have replaced the
    // other's snapshot, because they are not the same object.
    let old_bytes = fs::read_to_string(shard_path(dir.path(), old)).unwrap();
    let new_bytes = fs::read_to_string(shard_path(dir.path(), new)).unwrap();
    assert!(old_bytes.contains(&old_record.terminal.terminal_id.as_str()));
    assert!(!old_bytes.contains(&new_record.terminal.terminal_id.as_str()));
    assert!(new_bytes.contains(&new_record.terminal.terminal_id.as_str()));
    assert!(!new_bytes.contains(&old_record.terminal.terminal_id.as_str()));

    // Both reservations hold capacity in the one shared allocator.
    let allocator = fs::read_to_string(dir.path().join("daemon").join("allocations.json")).unwrap();
    assert!(allocator.contains(&old_record.terminal.terminal_id.as_str()));
    assert!(allocator.contains(&new_record.terminal.terminal_id.as_str()));

    // Every generation's records come back, whoever is reading.
    let hydrated = state(dir.path(), DaemonGeneration::new())
        .hydrate()
        .unwrap();
    assert_eq!(hydrated.terminals.records.len(), 2);
    assert_eq!(hydrated.interrupted, 2);
    assert!(hydrated.terminals.records.iter().all(|record| record.state
        == TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown)));
}

#[test]
fn a_legacy_store_is_adopted_from_its_own_bytes_and_retired_by_rename() {
    let dir = TempDir::new().unwrap();
    let legacy_owner = DaemonGeneration::new();
    let legacy = record(legacy_owner, TerminalRuntimeState::Running);
    let ended = record(legacy_owner, TerminalRuntimeState::Exited);
    write_legacy(
        dir.path(),
        "terminals.json",
        &serde_json::to_string(&snapshot(vec![legacy.clone(), ended.clone()])).unwrap(),
    );
    let daemon = dir.path().join("daemon");

    let migration = state(dir.path(), DaemonGeneration::new())
        .hydrate()
        .unwrap()
        .migration
        .unwrap();

    assert_eq!(migration.marker.schema, MIGRATION_SCHEMA);
    assert_eq!(migration.marker.generations, vec![legacy_owner.as_str()]);
    assert_eq!(migration.marker.adopted, 2);
    // The legacy identity proves nothing, so neither record is adopted as a live
    // child: both are non-spawnable safe failures.
    assert_eq!(migration.marker.unknown, 2);
    assert_eq!(migration.unknown.len(), 2);
    assert!(
        migration
            .unknown
            .iter()
            .any(|record| record.resource == legacy.terminal)
    );

    // One way: the bytes stay inspectable under a new name, and no build reads
    // them again.
    assert!(!daemon.join("terminals.json").exists());
    let retired = fs::read_to_string(daemon.join("terminals.json.migrated")).unwrap();
    assert!(retired.contains(&legacy.terminal.terminal_id.as_str()));
    let marker = fs::read_to_string(daemon.join("runtime-migration.json")).unwrap();
    assert!(marker.contains(MIGRATION_SCHEMA));
    assert!(marker.contains(&legacy_owner.as_str()));
    let adopted = fs::read_to_string(shard_path(dir.path(), legacy_owner)).unwrap();
    assert!(adopted.contains("ownership_unknown"));

    // A second start finds nothing to migrate and leaves the shard alone.
    let again = state(dir.path(), DaemonGeneration::new())
        .hydrate()
        .unwrap();
    assert!(again.migration.is_none());
    assert_eq!(again.terminals.records.len(), 2);
    assert_eq!(
        fs::read_to_string(shard_path(dir.path(), legacy_owner)).unwrap(),
        adopted
    );
}

#[test]
fn a_legacy_agent_store_is_adopted_and_counted_before_it_is_migrated() {
    let dir = TempDir::new().unwrap();
    let legacy_owner = DaemonGeneration::new();
    let legacy = agent_record(legacy_owner);
    write_legacy(
        dir.path(),
        "agents.json",
        &serde_json::to_string(&RuntimeStoreSnapshot {
            records: vec![legacy.clone()],
            ..RuntimeStoreSnapshot::default()
        })
        .unwrap(),
    );

    // Before the migration, the record still describes a PTY a cold transition
    // would destroy, so the census counts it.
    let archive = ShardArchiveFiles::new(dir.path()).unwrap();
    let live = census(&archive).unwrap();
    assert_eq!((live.agents, live.terminals), (1, 0));

    // A record read back from a store this process did not spawn for can never be
    // proved, whichever token it carries.
    assert!(
        UnprovenChildren
            .verified(legacy.process.as_ref().unwrap())
            .is_none()
    );
    let agents = ShardedAgentStore::new(state(dir.path(), DaemonGeneration::new()));
    let state = state(dir.path(), agents.state().owner());
    assert_ne!(state.owner(), legacy_owner);
    let hydrated = state.hydrate().unwrap();
    let migration = hydrated.migration.unwrap();
    assert_eq!(migration.marker.adopted, 1);
    // The fixed token cannot be re-observed, so the record is adopted as a
    // non-spawnable safe failure and holds no capacity.
    assert_eq!(migration.marker.unknown, 1);
    assert_eq!(migration.unknown[0].resource, legacy.runtime.terminal);
    assert_eq!(hydrated.agents.records.len(), 1);
    assert_eq!(hydrated.interrupted, 1);
    assert_eq!(census(&archive).unwrap().agents, 0);
}

#[test]
fn a_drained_generations_shard_is_removed_and_a_retained_one_is_kept() {
    let dir = TempDir::new().unwrap();
    let old = DaemonGeneration::new();
    let exited = record(old, TerminalRuntimeState::Exited);
    ShardedTerminalStore::new(state(dir.path(), old))
        .save(snapshot(vec![exited.clone()]))
        .unwrap();
    assert!(shard_path(dir.path(), old).exists());

    let active = state(dir.path(), DaemonGeneration::new());
    let retained: BTreeSet<String> =
        std::iter::once(exited.terminal.terminal_id.as_str()).collect();
    let limits = shipping_retention_limits();

    // While the active generation still answers for the record, its history stays.
    assert_eq!(active.collect(&retained, &limits).unwrap().1, 0);
    assert!(shard_path(dir.path(), old).exists());

    // Once nothing retains it, the whole document goes.
    assert_eq!(active.collect(&BTreeSet::new(), &limits).unwrap().1, 1);
    assert!(!shard_path(dir.path(), old).exists());
}

#[test]
fn a_census_reads_every_generation_and_the_unmigrated_legacy_stores() {
    let dir = TempDir::new().unwrap();
    let live_owner = DaemonGeneration::new();
    ShardedTerminalStore::new(state(dir.path(), live_owner))
        .save(snapshot(vec![record(
            live_owner,
            TerminalRuntimeState::Running,
        )]))
        .unwrap();
    let mut ended = agent_record(DaemonGeneration::new());
    ended.state = TerminalRuntimeState::Exited;
    write_legacy(
        dir.path(),
        "agents.json",
        &serde_json::to_string(&RuntimeStoreSnapshot {
            records: vec![ended],
            ..RuntimeStoreSnapshot::default()
        })
        .unwrap(),
    );
    let unmigrated = DaemonGeneration::new();
    write_legacy(
        dir.path(),
        "terminals.json",
        &serde_json::to_string(&snapshot(vec![
            record(unmigrated, TerminalRuntimeState::Running),
            // A record waiting to be reconciled owns no PTY any more.
            record(
                unmigrated,
                TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            ),
        ]))
        .unwrap(),
    );

    let archive = ShardArchiveFiles::new(dir.path()).unwrap();
    let live = census(&archive).unwrap();

    // A record whose child this process never observed is not live runtime in the
    // shard, and the legacy store is counted from its own states. A census never
    // migrates or reconciles what it counts.
    assert_eq!((live.agents, live.terminals), (0, 1));
    assert!(dir.path().join("daemon").join("agents.json").exists());
    assert!(dir.path().join("daemon").join("terminals.json").exists());
    assert!(
        !dir.path()
            .join("daemon")
            .join("runtime-migration.json")
            .exists()
    );
}

//! The production durable runtime state, on owner shards and the global
//! allocator.
//!
//! [`super::resources`] is the contract; this module is what puts the shipping
//! Agent and generic-terminal stores on it. The two whole-snapshot documents
//! (`agents.json`, `terminals.json`) were safe only while `daemon.lock`
//! guaranteed a single writer: two processes load the same bytes and the last
//! rename wins. Here each generation writes one document — its own shard — and
//! nothing else:
//!
//! ```text
//! shards/<G1>.json   contract state + G1's payload    written by G1 only
//! shards/<G2>.json   contract state + G2's payload    written by G2 only
//! allocations.json   capacity + producer operations   compare-and-swapped by both
//! ```
//!
//! A save therefore has two halves. The **payload** is the owner's own record set
//! in the vocabulary its runtime already speaks, kept opaque to the contract and
//! committed inside the same shard compare-and-swap. The **projection** is what
//! the contract needs from those records — who owns what, whether the OS confirms
//! the child, and how much capacity is held — and it is written in the order
//! [`super::resources::launch`] fixes: the global claim before the owner's
//! reservation, the operation's final after it.
//!
//! | durable record state | shard | allocator |
//! |---|---|---|
//! | reserved | reservation | claim taken (capacity gate) |
//! | running, OS confirms the exact process | running + verified identity | operation sealed spawned |
//! | running, OS cannot confirm it | ownership unknown | claim kept: a child may exist |
//! | spawn failed | ownership unknown | released exactly once: no child exists |
//! | exited, reclaimed | untouched | untouched: the exit path publishes it |
//!
//! Nothing here is reversible by an older build: a shard is a document the
//! whole-snapshot stores cannot read, so [`retire_legacy`] renames the legacy
//! document aside instead of deleting it, and a build that still reads
//! `agents.json` would find no state rather than stale state.

use std::io;

use serde_json::Value;
use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use crate::usecase::generation::ProcessIdentity;
use crate::usecase::generic_terminal::{TerminalStore, TerminalStoreSnapshot};
use crate::usecase::resources::allocator::{LaunchFailure, ResourceAllocator, ResourceKind};
use crate::usecase::resources::drain::ActiveConsumer;
use crate::usecase::resources::identity::{
    ChildIdentity, ChildObservation, ChildProcessProbe, IDENTITY_SOURCE_OS, observe_child,
};
use crate::usecase::resources::migration::{LegacyRuntimeRecord, UnknownRecord, adopt_legacy};
use crate::usecase::resources::retention::LogicalClock;
use crate::usecase::resources::shard::{OwnerShard, ResourceState, ShardDocument};
use crate::usecase::resources::{ResourceError, ResourceFailure};
use crate::usecase::runtime::{RuntimeStore, RuntimeStoreSnapshot};
use crate::usecase::terminal::TerminalRuntimeState;

/// The suffix a retired legacy store keeps, so the bytes stay inspectable and are
/// never read as live state again.
pub const RETIRED_LEGACY_SUFFIX: &str = ".migrated";

/// How many times a compare-and-swap is re-attempted after another writer
/// committed first.
///
/// A single attempt is the right contract for the launch protocol, where a lost
/// race means "somebody else decided this". Here it is not: two generations write
/// the *same* allocator legitimately and often, and a durable save that gave up on
/// the first collision would surface a store failure for a launch that is
/// perfectly admissible. Every phase re-reads and re-applies, so a retry is a
/// converged repeat rather than a second effect.
const CAS_ATTEMPTS: usize = 8;

/// Run one compare-and-swap until it is not simply losing a race.
fn with_retry<T>(
    mut attempt: impl FnMut() -> Result<T, ResourceFailure>,
) -> Result<T, ResourceFailure> {
    for _ in 1..CAS_ATTEMPTS {
        match attempt() {
            Err(failure) if failure.refusal() == Some(ResourceError::StaleRevision) => {}
            outcome => return outcome,
        }
    }
    attempt()
}

/// Read a legacy Agent store, refusing anything this build must not adopt.
///
/// # Errors
/// Returns [`ResourceError::Corrupt`] for bytes that are not a snapshot or whose
/// generation binding contradicts its records, and
/// [`ResourceError::UnknownSchema`] for a schema this build does not understand.
/// A refusal migrates nothing and leaves the legacy bytes exactly as they are.
pub fn read_legacy_agents(bytes: &str) -> Result<RuntimeStoreSnapshot, ResourceError> {
    let snapshot: RuntimeStoreSnapshot =
        serde_json::from_str(bytes).map_err(|_| ResourceError::Corrupt)?;
    snapshot
        .validate_schema()
        .map_err(|_| ResourceError::UnknownSchema)?;
    snapshot
        .validate_ownership()
        .map_err(|_| ResourceError::Corrupt)?;
    Ok(snapshot)
}

/// Read a legacy generic terminal store, refusing anything this build must not
/// adopt.
///
/// # Errors
/// Returns [`ResourceError::Corrupt`] for bytes that are not a snapshot and
/// [`ResourceError::UnknownSchema`] for an unknown schema.
pub fn read_legacy_terminals(bytes: &str) -> Result<TerminalStoreSnapshot, ResourceError> {
    let snapshot: TerminalStoreSnapshot =
        serde_json::from_str(bytes).map_err(|_| ResourceError::Corrupt)?;
    if snapshot.schema_version != TerminalStoreSnapshot::SCHEMA_VERSION {
        return Err(ResourceError::UnknownSchema);
    }
    Ok(snapshot)
}

/// What one durable runtime record means to the cross-generation contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectedState {
    /// Reserved durably; nothing has been spawned for it yet.
    Reserved,
    /// A child was spawned, and the record names the process it should be.
    Running(ProcessIdentity),
    /// A child may exist and the record cannot prove which one.
    Unknown,
    /// The spawn definitely produced no process.
    Failed,
    /// The record reached its end. The exit path owns that transition, because it
    /// is the only place the exit *status* is known; a save never invents one.
    Ended,
}

/// One durable runtime record, in the vocabulary the contract needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProjection {
    pub resource: TerminalRef,
    pub kind: ResourceKind,
    /// The producer operation this record was launched for.
    pub operation: OperationId,
    /// Canonical intent digest. Records written before the producer id reached
    /// the wire carry an empty digest and can never prove a replay.
    pub digest: String,
    pub state: ProjectedState,
}

/// What one durable save did to the cross-generation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SaveReport {
    pub reserved: usize,
    pub running: usize,
    pub unknown: usize,
    pub failed: usize,
    /// Records whose transition belongs to the exit path.
    pub ended: usize,
}

/// Opens the retained shards of a data directory.
///
/// It is a port because enumerating and binding the documents is real IO: the
/// production adapter reads `<data-dir>/daemon/shards`, and the tests bind
/// in-memory documents so every convergence case is driven without a filesystem.
pub trait ShardSource {
    /// Every generation that still has a retained shard.
    ///
    /// # Errors
    /// Returns the directory's read error. A source that cannot be enumerated is
    /// never treated as "no generation has state".
    fn generations(&self) -> io::Result<Vec<DaemonGeneration>>;

    /// Bind one generation's shard.
    ///
    /// # Errors
    /// Returns the error that kept the document from being bound.
    fn open(&self, generation: DaemonGeneration) -> io::Result<OwnerShard>;
}

/// The single writer of one generation's durable runtime state.
///
/// One of these exists per resource kind per process, and each is bound to the
/// shard of the generation that process *is*. It cannot be pointed at another
/// generation's shard: [`OwnerShard`] refuses a document with a different owner.
pub struct OwnerRuntimeState {
    kind: ResourceKind,
    shard: OwnerShard,
    allocator: ResourceAllocator,
    probe: Box<dyn ChildProcessProbe + Send>,
    clock: Box<dyn LogicalClock + Send>,
}

impl OwnerRuntimeState {
    /// Bind one kind's state for the generation `shard` belongs to.
    #[must_use]
    pub fn new(
        kind: ResourceKind,
        shard: OwnerShard,
        allocator: ResourceAllocator,
        probe: Box<dyn ChildProcessProbe + Send>,
        clock: Box<dyn LogicalClock + Send>,
    ) -> Self {
        Self {
            kind,
            shard,
            allocator,
            probe,
            clock,
        }
    }

    /// The generation this writer owns.
    #[must_use]
    pub fn owner(&self) -> DaemonGeneration {
        self.shard.owner()
    }

    /// Commit one owner's whole durable record set.
    ///
    /// The claims are taken first (authority leads the effect), the payload and
    /// every record transition land in one shard compare-and-swap, and the
    /// operations the shard now proves are sealed afterwards. A refusal in any
    /// phase leaves the durable documents exactly as they were, so a launch whose
    /// capacity is exhausted is refused *before* anything is spawned.
    ///
    /// # Errors
    /// Returns [`ResourceError::CapacityExhausted`] when a pool is full across
    /// every retained generation, any other contract refusal, or a store failure.
    pub fn commit(
        &self,
        payload: &Value,
        records: &[RuntimeProjection],
    ) -> Result<SaveReport, ResourceFailure> {
        let resolved = self.resolve(records);
        self.claim_capacity(&resolved)?;
        self.write_shard(payload, &resolved)?;
        self.seal_finals(&resolved)
    }

    /// Publish one child's exit and apply it, which is the only path that
    /// releases capacity for a child that ran.
    ///
    /// The owner is its own active consumer while it is the active generation, so
    /// the three steps of the handoff (publish, apply, reclaim) run here in order.
    /// A draining owner publishes only the first step; the active generation
    /// applies the rest ([`super::resources::drain`]).
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`] for a resource this owner never
    /// reserved, [`ResourceError::WrongState`] for a record that never held a
    /// child, or a store failure.
    pub fn publish_exit(&self, resource: &TerminalRef, status: i32) -> Result<(), ResourceFailure> {
        with_retry(|| {
            self.shard
                .update(|document| document.commit_exit(resource, status))
        })?;
        let published = self.shard.load()?.to_document();
        with_retry(|| ActiveConsumer::new(&self.allocator).consume(&published))?;
        let consumed = self.allocator.load()?.to_document();
        with_retry(|| {
            self.shard
                .update(|document| Ok(document.reclaim(&consumed)))
        })?;
        Ok(())
    }

    /// Resolve each record against the OS once, so the shard write and the
    /// sealing agree about which children are confirmed.
    fn resolve<'a>(&self, records: &'a [RuntimeProjection]) -> Vec<ResolvedRecord<'a>> {
        records
            .iter()
            .map(|projection| ResolvedRecord {
                verified: match &projection.state {
                    ProjectedState::Running(process) => {
                        verify_process(self.probe.as_ref(), process)
                    }
                    _ => None,
                },
                projection,
            })
            .collect()
    }

    /// L1: the global claim is durable before the owner's reservation is.
    ///
    /// Only a record that is reserved or running takes capacity. A record that
    /// can never spawn takes none, so a refusal here always describes a launch
    /// that has not happened yet.
    fn claim_capacity(&self, records: &[ResolvedRecord<'_>]) -> Result<(), ResourceFailure> {
        let owner = self.owner();
        let policy = self.allocator.policy();
        with_retry(|| {
            self.allocator.update(|document| {
                for resolved in records {
                    let projection = resolved.projection;
                    if !matches!(
                        projection.state,
                        ProjectedState::Reserved | ProjectedState::Running(_)
                    ) {
                        continue;
                    }
                    document.reserve(
                        &projection.operation,
                        &projection.digest,
                        projection.kind,
                        owner,
                        &projection.resource,
                        policy,
                    )?;
                }
                Ok(())
            })
        })?;
        Ok(())
    }

    /// L2 and L4: the payload and every record transition in one swap.
    fn write_shard(
        &self,
        payload: &Value,
        records: &[ResolvedRecord<'_>],
    ) -> Result<(), ResourceFailure> {
        let kind = self.kind;
        with_retry(|| {
            self.shard.update(|document| {
                document.set_payload(kind, payload.clone());
                for resolved in records {
                    apply_record(document, resolved)?;
                }
                Ok(())
            })
        })?;
        Ok(())
    }

    /// L5: the producer's answer becomes durable for everything the shard proves.
    fn seal_finals(&self, records: &[ResolvedRecord<'_>]) -> Result<SaveReport, ResourceFailure> {
        let now = self.clock.now();
        let (report, _) = with_retry(|| {
            self.allocator.update(|document| {
                let mut report = SaveReport::default();
                for resolved in records {
                    let operation = &resolved.projection.operation;
                    match resolved.projection.state {
                        ProjectedState::Reserved => report.reserved += 1,
                        ProjectedState::Running(_) if resolved.verified.is_some() => {
                            document.mark_spawned(operation, now)?;
                            report.running += 1;
                        }
                        ProjectedState::Running(_) | ProjectedState::Unknown => report.unknown += 1,
                        ProjectedState::Failed => {
                            // A definite failure releases the capacity its
                            // reservation took, exactly once and only if this build
                            // is the one that took it.
                            if document
                                .operation(operation)
                                .is_some_and(|record| !record.outcome.is_final())
                            {
                                document.mark_failed(operation, LaunchFailure::Spawn, now)?;
                            }
                            report.failed += 1;
                        }
                        ProjectedState::Ended => report.ended += 1,
                    }
                }
                Ok(report)
            })
        })?;
        Ok(report)
    }
}

/// One record together with the child the OS confirmed for it, if any.
struct ResolvedRecord<'a> {
    projection: &'a RuntimeProjection,
    verified: Option<ChildIdentity>,
}

/// Apply one record's meaning to the shard.
///
/// It is deliberately not a closure inside [`OwnerRuntimeState::write_shard`]:
/// one branch table, one compiled copy, one place to read the mapping.
fn apply_record(
    document: &mut ShardDocument,
    resolved: &ResolvedRecord<'_>,
) -> Result<(), ResourceError> {
    let projection = resolved.projection;
    if projection.state == ProjectedState::Ended {
        // An exit is published from the observation that carries its status, and
        // a reclaimed record may already be forgotten. Re-reserving it here would
        // resurrect a resource whose capacity was released.
        return Ok(());
    }
    document.reserve(
        &projection.operation,
        &projection.digest,
        projection.kind,
        &projection.resource,
    )?;
    let unknown = document
        .resource(&projection.resource)
        .is_some_and(|entry| entry.state == ResourceState::OwnershipUnknown);
    match (&projection.state, &resolved.verified) {
        // A record that already lost its proof of ownership never regains it: the
        // window in which the OS could have answered has passed.
        (_, _) if unknown => Ok(()),
        (ProjectedState::Running(_), Some(identity)) => {
            document.record_spawn(&projection.resource, identity)
        }
        (ProjectedState::Running(_) | ProjectedState::Unknown | ProjectedState::Failed, _) => {
            document.mark_ownership_unknown(&projection.resource)
        }
        // A reservation is already what the shard records, and `Ended` returned
        // above — neither has a transition left to make.
        (ProjectedState::Reserved | ProjectedState::Ended, _) => Ok(()),
    }
}

/// Whether the OS still reports the exact process a record names.
///
/// A durable record cannot say where its `start_identity` came from, so the
/// answer is not read out of the record: it is re-observed. A token the platform
/// confirms for that pid *is* the process the record describes; a fixed legacy
/// string, a reused pid, or an unreadable platform confirms nothing and can never
/// become spawn, signal, or capacity authority.
#[must_use]
pub fn verify_process(
    probe: &dyn ChildProcessProbe,
    process: &ProcessIdentity,
) -> Option<ChildIdentity> {
    let candidate = ChildIdentity {
        pid: process.pid,
        process_group: process.process_group,
        source: IDENTITY_SOURCE_OS.to_owned(),
        start_identity: process.start_identity.clone(),
    };
    match observe_child(probe, &candidate) {
        ChildObservation::Exact => Some(candidate),
        ChildObservation::Gone | ChildObservation::Reused | ChildObservation::Unknown => None,
    }
}

/// Project the Agent runtime records this generation owns.
#[must_use]
pub fn project_agents(
    snapshot: &RuntimeStoreSnapshot,
    owner: DaemonGeneration,
) -> Vec<RuntimeProjection> {
    snapshot
        .records
        .iter()
        .filter(|record| record.runtime.terminal.daemon_generation == owner)
        .map(|record| RuntimeProjection {
            resource: record.runtime.terminal.clone(),
            kind: ResourceKind::Agent,
            operation: record.operation.operation_id,
            digest: record.semantic_key.clone().unwrap_or_default(),
            state: projected_state(record.state, record.process.as_ref()),
        })
        .collect()
}

/// Project the generic terminal records this generation owns.
#[must_use]
pub fn project_terminals(
    snapshot: &TerminalStoreSnapshot,
    owner: DaemonGeneration,
) -> Vec<RuntimeProjection> {
    snapshot
        .records
        .iter()
        .filter(|record| record.terminal.daemon_generation == owner)
        .map(|record| RuntimeProjection {
            resource: record.terminal.clone(),
            kind: ResourceKind::Terminal,
            operation: record.operation.operation_id,
            digest: record.launch_digest.clone().unwrap_or_default(),
            state: projected_state(record.state, record.process.as_ref()),
        })
        .collect()
}

fn projected_state(
    state: TerminalRuntimeState,
    process: Option<&ProcessIdentity>,
) -> ProjectedState {
    match state {
        TerminalRuntimeState::Reserved => ProjectedState::Reserved,
        TerminalRuntimeState::Running => process.map_or(ProjectedState::Unknown, |process| {
            ProjectedState::Running(process.clone())
        }),
        TerminalRuntimeState::SpawnFailed => ProjectedState::Failed,
        TerminalRuntimeState::ReconcileRequired(_) => ProjectedState::Unknown,
        TerminalRuntimeState::Exited | TerminalRuntimeState::Reclaimed => ProjectedState::Ended,
    }
}

/// The Agent snapshot restricted to one owner, which is the only part of it that
/// generation may write.
#[must_use]
pub fn owned_agents(
    snapshot: &RuntimeStoreSnapshot,
    owner: DaemonGeneration,
) -> RuntimeStoreSnapshot {
    let mut scoped = snapshot.clone();
    scoped
        .records
        .retain(|record| record.runtime.terminal.daemon_generation == owner);
    scoped
        .generation
        .terminals
        .retain(|ownership| ownership.terminal.daemon_generation == owner);
    scoped
}

/// The terminal snapshot restricted to one owner.
#[must_use]
pub fn owned_terminals(
    snapshot: &TerminalStoreSnapshot,
    owner: DaemonGeneration,
) -> TerminalStoreSnapshot {
    let mut scoped = snapshot.clone();
    scoped
        .records
        .retain(|record| record.terminal.daemon_generation == owner);
    scoped
}

/// The durable Agent runtime store, backed by one owner shard.
pub struct AgentShardStore(OwnerRuntimeState);

impl AgentShardStore {
    /// Bind the store of the generation `state` owns.
    #[must_use]
    pub fn new(state: OwnerRuntimeState) -> Self {
        Self(state)
    }
}

impl RuntimeStore for AgentShardStore {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        let owner = self.0.owner();
        let records = project_agents(&snapshot, owner);
        let payload = serde_json::to_value(owned_agents(&snapshot, owner)).map_err(|_| ())?;
        self.0
            .commit(&payload, &records)
            .map(|_| ())
            .map_err(|_| ())
    }
}

/// The durable generic terminal store, backed by one owner shard.
pub struct TerminalShardStore(OwnerRuntimeState);

impl TerminalShardStore {
    /// Bind the store of the generation `state` owns.
    #[must_use]
    pub fn new(state: OwnerRuntimeState) -> Self {
        Self(state)
    }
}

impl TerminalStore for TerminalShardStore {
    fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), ()> {
        let owner = self.0.owner();
        let records = project_terminals(&snapshot, owner);
        let payload = serde_json::to_value(owned_terminals(&snapshot, owner)).map_err(|_| ())?;
        self.0
            .commit(&payload, &records)
            .map(|_| ())
            .map_err(|_| ())
    }
}

/// Read one kind's payload out of every retained shard.
///
/// A payload this build cannot parse fails the whole read: a runtime that starts
/// with half of the durable records would spawn into state it cannot see.
///
/// # Errors
/// Returns [`ResourceError::Corrupt`] for an unparsable payload, the shard's own
/// validation refusal, or a store failure.
fn retained_payloads(
    source: &dyn ShardSource,
    kind: ResourceKind,
) -> Result<Vec<(DaemonGeneration, Value)>, ResourceFailure> {
    let mut payloads = Vec::new();
    for generation in source.generations()? {
        let shard = source.open(generation)?;
        let document = shard.load()?.to_document();
        if let Some(payload) = document.payload(kind) {
            payloads.push((document.owner, payload.clone()));
        }
    }
    Ok(payloads)
}

/// Hydrate the Agent runtime snapshot from every retained shard.
///
/// The merged snapshot is reconciled exactly as a restart reconciles one: a
/// record whose owning process is gone keeps its history and loses its claim to a
/// live PTY. Reconciling in memory is deliberate — the records of another
/// generation stay in that generation's document, which this process never
/// writes.
///
/// # Errors
/// Returns [`ResourceError::Corrupt`] for a payload this build cannot parse or
/// one that names a foreign owner, or a store failure.
pub fn hydrate_agents(
    source: &dyn ShardSource,
) -> Result<(RuntimeStoreSnapshot, usize), ResourceFailure> {
    let mut merged = RuntimeStoreSnapshot::default();
    for (owner, payload) in retained_payloads(source, ResourceKind::Agent)? {
        let snapshot: RuntimeStoreSnapshot =
            serde_json::from_value(payload).map_err(|_| ResourceError::Corrupt)?;
        if snapshot
            .records
            .iter()
            .any(|record| record.runtime.terminal.daemon_generation != owner)
        {
            return Err(ResourceError::Corrupt.into());
        }
        merged.records.extend(snapshot.records);
    }
    Ok(merged.reconcile_after_daemon_restart())
}

/// Hydrate the generic terminal snapshot from every retained shard.
///
/// # Errors
/// Returns [`ResourceError::Corrupt`] for a payload this build cannot parse, one
/// that names a foreign owner, or one the terminal snapshot itself refuses, or a
/// store failure.
pub fn hydrate_terminals(
    source: &dyn ShardSource,
) -> Result<(TerminalStoreSnapshot, usize), ResourceFailure> {
    let mut merged = TerminalStoreSnapshot::default();
    for (owner, payload) in retained_payloads(source, ResourceKind::Terminal)? {
        let snapshot: TerminalStoreSnapshot =
            serde_json::from_value(payload).map_err(|_| ResourceError::Corrupt)?;
        if snapshot
            .records
            .iter()
            .any(|record| record.terminal.daemon_generation != owner)
        {
            return Err(ResourceError::Corrupt.into());
        }
        merged.records.extend(snapshot.records);
    }
    merged
        .reconcile_after_daemon_restart()
        .map_err(|_| ResourceError::Corrupt.into())
}

/// What adopting the legacy stores did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdoptionSummary {
    /// Generations whose shard was written from legacy records.
    pub owners: usize,
    /// Records adopted as live resources.
    pub adopted: usize,
    /// Records kept as `ownership_unknown`, with the reason each could not be
    /// proved.
    pub unknown: Vec<UnknownRecord>,
}

/// Adopt a legacy Agent store into the shards of the generations it names.
///
/// # Errors
/// Returns a contract refusal or a store failure. Nothing partial is *lost*: the
/// per-owner writes are idempotent, so a repeated pass after a crash adopts the
/// same records once.
pub fn migrate_agents(
    source: &dyn ShardSource,
    allocator: &ResourceAllocator,
    probe: &dyn ChildProcessProbe,
    clock: &dyn LogicalClock,
    legacy: &RuntimeStoreSnapshot,
) -> Result<AdoptionSummary, ResourceFailure> {
    let records: Vec<LegacyRuntimeRecord> = legacy
        .records
        .iter()
        .map(|record| LegacyRuntimeRecord {
            resource: record.runtime.terminal.clone(),
            kind: ResourceKind::Agent,
            operation: Some(record.operation.operation_id),
            digest: record.semantic_key.clone(),
            process: record
                .process
                .as_ref()
                .map(|process| legacy_identity(probe, process)),
            live: owns_child(record.state),
        })
        .collect();
    migrate(
        source,
        allocator,
        clock,
        ResourceKind::Agent,
        &records,
        &|owner| serde_json::to_value(owned_agents(legacy, owner)),
    )
}

/// Adopt a legacy generic terminal store into the shards of the generations it
/// names.
///
/// # Errors
/// Returns a contract refusal or a store failure.
pub fn migrate_terminals(
    source: &dyn ShardSource,
    allocator: &ResourceAllocator,
    probe: &dyn ChildProcessProbe,
    clock: &dyn LogicalClock,
    legacy: &TerminalStoreSnapshot,
) -> Result<AdoptionSummary, ResourceFailure> {
    let records: Vec<LegacyRuntimeRecord> = legacy
        .records
        .iter()
        .map(|record| LegacyRuntimeRecord {
            resource: record.terminal.clone(),
            kind: ResourceKind::Terminal,
            operation: Some(record.operation.operation_id),
            digest: record.launch_digest.clone(),
            process: record
                .process
                .as_ref()
                .map(|process| legacy_identity(probe, process)),
            live: owns_child(record.state),
        })
        .collect();
    migrate(
        source,
        allocator,
        clock,
        ResourceKind::Terminal,
        &records,
        &|owner| serde_json::to_value(owned_terminals(legacy, owner)),
    )
}

/// Whether a legacy record claimed to still hold a child.
const fn owns_child(state: TerminalRuntimeState) -> bool {
    matches!(
        state,
        TerminalRuntimeState::Reserved | TerminalRuntimeState::Running
    )
}

/// A legacy record's child, verified against the OS or explicitly marked
/// unverifiable.
fn legacy_identity(probe: &dyn ChildProcessProbe, process: &ProcessIdentity) -> ChildIdentity {
    verify_process(probe, process)
        .unwrap_or_else(|| ChildIdentity::unverifiable(process.pid, process.start_identity.clone()))
}

/// Adopt legacy records into each named generation's shard, one owner at a time.
///
/// | crash boundary | durable state | recovery |
/// |---|---|---|
/// | before the first owner | legacy document only | a repeated pass starts over |
/// | between two owners | some shards written, legacy document intact | a repeated pass adopts the rest and skips what exists |
/// | after the last owner | every shard written, legacy document intact | a repeated pass changes nothing; the caller retires the document |
/// | after retiring | shards are the only state | nothing left to adopt |
///
/// The claims are written before the shard of each owner, in the order a launch
/// uses, so a crash between them leaves a claim whose reservation the next pass
/// completes — never a reservation no claim backs.
fn migrate(
    source: &dyn ShardSource,
    allocator: &ResourceAllocator,
    clock: &dyn LogicalClock,
    kind: ResourceKind,
    records: &[LegacyRuntimeRecord],
    payload_of: &dyn Fn(DaemonGeneration) -> serde_json::Result<Value>,
) -> Result<AdoptionSummary, ResourceFailure> {
    let mut summary = AdoptionSummary::default();
    for owner in owners_of(records) {
        let report = adopt_legacy(owner, records);
        let live: Vec<_> = report
            .shard
            .resources
            .iter()
            .filter(|entry| entry.state.is_live())
            .cloned()
            .collect();
        // A record adopted as live is a child the OS confirmed, so its capacity is
        // held and its operation is already answered.
        let now = clock.now();
        let policy = allocator.policy();
        with_retry(|| {
            allocator.update(|document| {
                for entry in &live {
                    document.reserve(
                        &entry.operation,
                        &entry.digest,
                        entry.kind,
                        owner,
                        &entry.resource,
                        policy,
                    )?;
                    document.mark_spawned(&entry.operation, now)?;
                }
                Ok(())
            })
        })?;
        let payload = payload_of(owner).map_err(|_| ResourceError::Corrupt)?;
        let shard = source.open(owner)?;
        with_retry(|| {
            shard.update(|document| {
                document.set_payload(kind, payload.clone());
                for entry in &report.shard.resources {
                    if document.resource(&entry.resource).is_none() {
                        document.resources.push(entry.clone());
                    }
                }
                Ok(())
            })
        })?;
        summary.owners += 1;
        summary.adopted += live.len();
        summary.unknown.extend(
            report
                .unknown
                .iter()
                .filter(|unknown| {
                    // A record of another generation is reported by that generation's
                    // own pass, so it is not counted twice.
                    unknown.resource.daemon_generation == owner
                })
                .cloned(),
        );
    }
    Ok(summary)
}

/// The generations a legacy record set names, in first-seen order.
fn owners_of(records: &[LegacyRuntimeRecord]) -> Vec<DaemonGeneration> {
    let mut owners: Vec<DaemonGeneration> = Vec::new();
    for record in records {
        let owner = record.resource.daemon_generation;
        if !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    owners
}

/// What converging one dead owner's state did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CollectionReport {
    /// Published events the active generation applied in this pass.
    pub consumed: usize,
    /// Live records that lost their claim to a live PTY.
    pub unknown: usize,
    /// Claims released against an OS observation that the child is gone.
    pub released: usize,
    /// Claims kept because the child is still running, or nothing could be
    /// proved.
    pub retained: usize,
    /// Outbox entries reclaimed on the dead owner's behalf.
    pub reclaimed: usize,
}

/// Converge the runtime state of a generation whose process is **proved dead**.
///
/// A dead owner publishes nothing more, so somebody has to finish its work or its
/// records stay live and its capacity stays held for the life of the data
/// directory. That is only safe with the proof the caller brings: while a
/// generation may still be running, its shard is its own and nobody else writes
/// it. The shipping `serve` proves it by holding the single-instance lock, which
/// no other daemon process can hold at the same time.
///
/// Every decision is evidence, never inference:
///
/// | shard record | evidence | outcome |
/// |---|---|---|
/// | exited, published | the owner's own event | applied, capacity released once |
/// | running, child proved gone | OS observation | ownership unknown, capacity released |
/// | running, child still alive | OS observation | ownership unknown, capacity kept |
/// | running, nothing provable | none | ownership unknown, capacity kept |
/// | reserved | none: a child may have been spawned unrecorded | ambiguous, capacity kept |
///
/// # Errors
/// Returns a contract refusal or a store failure.
pub fn collect_dead_owner(
    shard: &OwnerShard,
    allocator: &ResourceAllocator,
    probe: &dyn ChildProcessProbe,
    clock: &dyn LogicalClock,
) -> Result<CollectionReport, ResourceFailure> {
    let owner = shard.owner();
    let mut report = CollectionReport::default();
    let published = shard.load()?.to_document();
    let consumed = with_retry(|| ActiveConsumer::new(allocator).consume(&published))?;
    report.consumed = consumed.applied;
    let now = clock.now();
    let live: Vec<_> = published
        .resources
        .iter()
        .filter(|entry| entry.state.is_live())
        .map(|entry| {
            (
                entry.clone(),
                entry
                    .process
                    .as_ref()
                    .map_or(ChildObservation::Unknown, |process| {
                        observe_child(probe, process)
                    }),
            )
        })
        .collect();
    for (entry, observation) in &live {
        with_retry(|| {
            allocator.update(|document| {
                match entry.state {
                    // A reservation without a running record may have spawned a
                    // child this owner never got to write down. Nothing may be
                    // inferred from it, so the operation ends ambiguous and keeps
                    // its capacity.
                    ResourceState::Reserved => document.mark_ambiguous(&entry.operation, now),
                    _ if observation.is_definitely_gone() => {
                        document.mark_spawned(&entry.operation, now)?;
                        document.release_gone(owner, &entry.resource)
                    }
                    _ => document.mark_spawned(&entry.operation, now),
                }
            })
        })?;
        if entry.state == ResourceState::Reserved || !observation.is_definitely_gone() {
            report.retained += 1;
        } else {
            report.released += 1;
        }
    }
    let (unknown, _) = with_retry(|| {
        shard.update(|document| {
            let mut unknown = 0;
            for (entry, _) in &live {
                document.mark_ownership_unknown(&entry.resource)?;
                unknown += 1;
            }
            Ok(unknown)
        })
    })?;
    report.unknown = unknown;
    let ledger = allocator.load()?.to_document();
    let (reclaimed, _) = with_retry(|| shard.update(|document| Ok(document.reclaim(&ledger))))?;
    report.reclaimed = reclaimed;
    Ok(report)
}

/// How much live runtime one generation still owns, per kind.
///
/// It reads the contract state rather than the payload: a record only counts as
/// live while the shard says this generation may still act on it.
#[must_use]
pub fn live_of(document: &ShardDocument, kind: ResourceKind) -> usize {
    document
        .resources
        .iter()
        .filter(|entry| entry.kind == kind && entry.state.is_live())
        .count()
}

/// The live runtime of every retained generation, per kind.
///
/// # Errors
/// Returns the shard's validation refusal or a store failure. A census that
/// cannot be taken is never reported as "nothing is live".
pub fn live_census(source: &dyn ShardSource) -> Result<(usize, usize), ResourceFailure> {
    let mut agents = 0;
    let mut terminals = 0;
    for generation in source.generations()? {
        let document = source.open(generation)?.load()?.to_document();
        agents += live_of(&document, ResourceKind::Agent);
        terminals += live_of(&document, ResourceKind::Terminal);
    }
    Ok((agents, terminals))
}

#[cfg(test)]
mod tests;

//! Production's durable runtime state, on owner shards and one global allocator.
//!
//! [`shard`](super::shard) and [`allocator`](super::allocator) define *how* two
//! daemon processes can own runtime state at the same time. This module is what
//! puts the shipping Agent and generic-terminal stores on that contract, and it is
//! the only place that knows both vocabularies:
//!
//! ```text
//!  before                                   after
//!  agents.json    whole snapshot            shards/<G>.json   one writer per generation
//!  terminals.json whole snapshot            allocations.json  capacity + producer ledger
//! ```
//!
//! Three rules make the move safe rather than merely different.
//!
//! **One writer per document.** A [`ShardedRuntimeState`] writes exactly one
//! shard — its own generation's — and refuses to project a record belonging to
//! another generation. A draining owner and a new active owner therefore never
//! write the same path, which is what the whole-snapshot stores could not
//! promise.
//!
//! **The authority leads the effect.** Capacity is claimed in the shared
//! allocator *before* the shard reservation is committed, and the shipping
//! coordinators already persist their reservation before spawning
//! ([`crate::usecase::runtime`]). A pool that is full across every retained
//! generation therefore refuses the save that precedes the spawn, so a refusal is
//! spawn effect zero.
//!
//! **Nothing is inferred from a record that cannot prove itself.** A production
//! record only becomes a live shard resource when *this* process observed its
//! child ([`IdentityAuthority`]). Everything else — a legacy fixed identity, a
//! record recovered after a restart, an ambiguous spawn — becomes
//! [`ResourceState::OwnershipUnknown`], which holds its capacity and is never
//! spawned, signalled, or released.
//!
//! The record itself travels as an opaque
//! [`payload`](super::shard::ShardResource::payload) on the shard resource, so one
//! compare-and-swap commits a record together with the state it describes.
//!
//! The migration from the legacy stores is one way and is described in
//! [`migrate_legacy`].

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use serde::Serialize;
use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use crate::usecase::generation::{GenerationRole, ProcessIdentity};
use crate::usecase::generic_terminal::{
    DurableTerminalRecord, TerminalStore, TerminalStoreSnapshot,
};
use crate::usecase::resources::allocator::{
    AllocatorDocument, CapacityPolicy, LaunchFailure, ResourceAllocator, ResourceKind,
};
use crate::usecase::resources::drain::ActiveConsumer;
use crate::usecase::resources::identity::ChildIdentity;
use crate::usecase::resources::migration::{LegacyRuntimeRecord, UnknownRecord, adopt_legacy};
use crate::usecase::resources::retention::{
    GcReport, LogicalClock, RetentionLimits, collect_garbage,
};
use crate::usecase::resources::shard::{
    OwnerShard, ShardDocument, ShardResource, retired_collectable,
};
use crate::usecase::resources::{CasDocument, CasFile, ResourceError, ResourceFailure};
use crate::usecase::runtime::{DurableRuntimeRecord, RuntimeStore, RuntimeStoreSnapshot};
use crate::usecase::terminal::TerminalRuntimeState;

/// The schema of the one-way migration marker.
pub const MIGRATION_SCHEMA: &str = "usagi-runtime-migration-v1";

/// The shipping bounds of the operation ledger, in whole seconds.
///
/// | bound | value | what it promises |
/// |---|---|---|
/// | records | 4096 | the hard cap; reaching it refuses fresh launches rather than evicting |
/// | bytes | 4 MiB | the same cap, measured on the serialized document |
/// | age | 7 days | a collectable final older than this is collected |
/// | window | 1 hour | inside it, the same producer operation replays its full exact answer |
/// | horizon | 30 days | an expired id keeps its typed refusal for at least this long |
#[must_use]
pub fn shipping_retention_limits() -> RetentionLimits {
    RetentionLimits::new(4096, 4 << 20, 7 * 24 * 3600, 3600, 30 * 24 * 3600)
}

/// The process-local proof that a child was observed at spawn time.
///
/// Verifiability cannot be read back out of a durable record: a stored token is
/// just bytes, and a legacy build stored a fixed string. So the only process that
/// may call a child its own is the one that observed the OS while spawning it,
/// and that knowledge deliberately does not survive a restart — a recovered
/// record is `identity_unknown`, exactly as the shipping reconcile already
/// reports it.
pub trait IdentityAuthority {
    /// The OS-verified identity of `process`, when this process observed it.
    fn verified(&self, process: &ProcessIdentity) -> Option<ChildIdentity>;
}

/// An authority that can prove nothing, for a process that spawned no child yet.
pub struct UnprovenChildren;

impl IdentityAuthority for UnprovenChildren {
    fn verified(&self, _process: &ProcessIdentity) -> Option<ChildIdentity> {
        None
    }
}

/// The state a production record projects onto.
///
/// `Running` carries the observed child, so a live shard resource cannot even be
/// *spelled* without the proof it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectedState {
    /// Durably reserved, nothing spawned yet.
    Reserved,
    /// A child this process observed is running.
    Running(ChildIdentity),
    /// The child is gone. Capacity is released exactly once, through the owner's
    /// outbox.
    Exited,
    /// No child was ever created. The producer operation is a definite failure.
    SpawnFailed,
    /// The record exists and its ownership cannot be proved. It keeps its
    /// capacity and is never acted on.
    Unproven,
}

impl ProjectedState {
    /// Whether a record in this state holds a capacity claim.
    #[must_use]
    pub fn holds_capacity(&self) -> bool {
        matches!(self, Self::Reserved | Self::Running(_) | Self::Unproven)
    }
}

/// One production record in the shard's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProjection {
    pub resource: TerminalRef,
    pub kind: ResourceKind,
    pub operation: OperationId,
    /// Canonical intent digest, namespaced by pool so one producer id can never
    /// silently replay the other kind's answer.
    pub digest: String,
    pub state: ProjectedState,
    /// The production record, serialized verbatim.
    pub payload: String,
}

/// Project one Agent runtime record.
#[must_use]
pub fn project_agent(
    record: &DurableRuntimeRecord,
    identity: &dyn IdentityAuthority,
) -> RuntimeProjection {
    RuntimeProjection {
        resource: record.runtime.terminal.clone(),
        kind: ResourceKind::Agent,
        operation: record.operation.operation_id,
        digest: digest_of(
            ResourceKind::Agent,
            record.semantic_key.as_deref().unwrap_or_default(),
        ),
        state: projected_state(
            record.state,
            record
                .process
                .as_ref()
                .and_then(|process| identity.verified(process)),
        ),
        payload: serde_json::to_string(record).unwrap_or_default(),
    }
}

/// Project one generic terminal record.
#[must_use]
pub fn project_terminal(
    record: &DurableTerminalRecord,
    identity: &dyn IdentityAuthority,
) -> RuntimeProjection {
    RuntimeProjection {
        resource: record.terminal.clone(),
        kind: ResourceKind::Terminal,
        operation: record.operation.operation_id,
        digest: digest_of(
            ResourceKind::Terminal,
            record.launch_digest.as_deref().unwrap_or_default(),
        ),
        state: projected_state(
            record.state,
            record
                .process
                .as_ref()
                .and_then(|process| identity.verified(process)),
        ),
        payload: serde_json::to_string(record).unwrap_or_default(),
    }
}

/// The pool-namespaced canonical digest. Both stores key the same ledger, so a
/// producer id reused across pools must conflict loudly instead of replaying.
fn digest_of(kind: ResourceKind, digest: &str) -> String {
    format!("{}:{digest}", kind.pool())
}

/// Map one shipping runtime state onto the shard vocabulary.
///
/// `Running` is the only state that needs proof, and without it the record is
/// unproven rather than live — that is the whole fail-closed rule in one place.
fn projected_state(state: TerminalRuntimeState, observed: Option<ChildIdentity>) -> ProjectedState {
    match (state, observed) {
        (TerminalRuntimeState::Reserved, _) => ProjectedState::Reserved,
        (TerminalRuntimeState::Running, Some(identity)) => ProjectedState::Running(identity),
        (TerminalRuntimeState::Running | TerminalRuntimeState::ReconcileRequired(_), _) => {
            ProjectedState::Unproven
        }
        (TerminalRuntimeState::Exited | TerminalRuntimeState::Reclaimed, _) => {
            ProjectedState::Exited
        }
        (TerminalRuntimeState::SpawnFailed, _) => ProjectedState::SpawnFailed,
    }
}

/// What one commit did, for the operator log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CommitReport {
    /// Records projected into this process's own shard.
    pub owned: usize,
    /// Records left alone because another generation owns them.
    pub foreign: usize,
    /// Capacity claims released for a retired generation's terminated record.
    pub released: usize,
    /// Outbox events applied by this process as the active consumer.
    pub consumed: usize,
}

/// The two legacy whole-snapshot documents, as raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LegacySnapshots {
    pub agents: Option<String>,
    pub terminals: Option<String>,
}

impl LegacySnapshots {
    /// Whether anything is left to migrate.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_none() && self.terminals.is_none()
    }
}

/// What the one-way migration adopted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationMarker {
    pub schema: String,
    /// Generations whose shard was built from the legacy stores.
    pub generations: Vec<String>,
    pub adopted: usize,
    pub unknown: usize,
}

/// The result of a migration pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub marker: MigrationMarker,
    /// Records kept as `OwnershipUnknown`, with the reason each one could not be
    /// proved.
    pub unknown: Vec<UnknownRecord>,
}

/// The durable directory the shards, the legacy stores, and the marker live in.
///
/// Every method is byte level on purpose: the decisions stay in this module and
/// the filesystem adapter stays a seam that a test can replace entirely
/// ([`crate::infrastructure::resource_store`]).
pub trait ShardArchive {
    /// The raw document of every retained shard.
    ///
    /// # Errors
    /// Returns an error when the shard directory cannot be read.
    fn documents(&self) -> io::Result<Vec<String>>;

    /// A compare-and-swap seam for one generation's shard.
    ///
    /// # Errors
    /// Returns an error when the shard cannot be bound.
    fn shard(&self, owner: DaemonGeneration) -> io::Result<Box<dyn CasFile + Send>>;

    /// Remove a fully drained generation's shard.
    ///
    /// # Errors
    /// Returns an error when the shard cannot be removed.
    fn collect(&self, owner: DaemonGeneration) -> io::Result<()>;

    /// The legacy stores, empty once they have been sealed.
    ///
    /// # Errors
    /// Returns an error when a legacy store exists but cannot be read.
    fn legacy(&self) -> io::Result<LegacySnapshots>;

    /// Record the migration and take the legacy stores out of service. This is
    /// the one-way step: afterwards no build reads them again.
    ///
    /// # Errors
    /// Returns an error when the marker cannot be written or a legacy store
    /// cannot be retired.
    fn seal_legacy(&self, marker: &str) -> io::Result<()>;
}

/// One generation's durable runtime state.
///
/// It owns its own shard and a handle on the shared allocator. Two of these may
/// exist in one process (one per production store) because every write is a
/// compare-and-swap: they converge on the same document instead of replacing each
/// other's copy of it.
pub struct ShardedRuntimeState {
    owner: DaemonGeneration,
    role: GenerationRole,
    shard: OwnerShard,
    allocator: ResourceAllocator,
    archive: Box<dyn ShardArchive + Send>,
    identity: Box<dyn IdentityAuthority + Send>,
    clock: Box<dyn LogicalClock + Send>,
}

impl ShardedRuntimeState {
    /// Bind this process's generation to its shard and the shared allocator.
    ///
    /// The seams are trait objects rather than type parameters, so the production
    /// adapters and the in-memory fakes share exactly one compiled copy of every
    /// transition below.
    ///
    /// # Errors
    /// Returns an error when this generation's shard cannot be bound.
    pub fn new(
        owner: DaemonGeneration,
        role: GenerationRole,
        allocator: ResourceAllocator,
        archive: Box<dyn ShardArchive + Send>,
        identity: Box<dyn IdentityAuthority + Send>,
        clock: Box<dyn LogicalClock + Send>,
    ) -> io::Result<Self> {
        let shard = OwnerShard::new(archive.shard(owner)?, owner);
        Ok(Self {
            owner,
            role,
            shard,
            allocator,
            archive,
            identity,
            clock,
        })
    }

    /// The generation this state writes for.
    #[must_use]
    pub fn owner(&self) -> DaemonGeneration {
        self.owner
    }

    /// The identity authority, so a caller can project records itself.
    #[must_use]
    pub fn identity(&self) -> &dyn IdentityAuthority {
        self.identity.as_ref()
    }

    /// Commit the owner's truth about one resource kind.
    ///
    /// The order is fixed and is the same one [`super::launch`] documents: the
    /// shared claim first, then the owner's shard, then the exits this process is
    /// the active consumer for.
    ///
    /// # Errors
    /// Returns [`ResourceError::CapacityExhausted`] when a pool is full across
    /// every retained generation — before any spawn, so the refusal has no effect
    /// — [`ResourceError::OperationConflict`] for a producer id reused with a
    /// different intent, or a store failure.
    pub fn commit(
        &self,
        kind: ResourceKind,
        projections: &[RuntimeProjection],
    ) -> Result<CommitReport, ResourceFailure> {
        let (owned, foreign): (Vec<_>, Vec<_>) = projections
            .iter()
            .partition(|projection| projection.resource.daemon_generation == self.owner);
        let mut report = CommitReport {
            owned: owned.len(),
            foreign: foreign.len(),
            ..CommitReport::default()
        };
        self.claim(&owned)?;
        self.project(kind, &owned)?;
        report.released = self.release_retired(&foreign)?;
        report.consumed = self.drain()?;
        Ok(report)
    }

    /// Read every retained generation's records, adopting the legacy stores first
    /// when they have not been migrated yet.
    ///
    /// The returned snapshots are the shipping ones, reconciled exactly as a
    /// restart reconciles them today: a record whose child this process never
    /// observed comes back as `identity_unknown`, visible and non-spawnable.
    ///
    /// # Errors
    /// Returns [`ResourceError::Corrupt`] or [`ResourceError::UnknownSchema`] for
    /// durable state this build must not act on, or a store failure. Startup then
    /// fails closed rather than launching against half-understood state.
    pub fn hydrate(&self) -> Result<HydratedState, ResourceFailure> {
        let migration = self.migrate_legacy()?;
        let shards = self.retained()?;
        let mut agents = Vec::new();
        let mut terminals = Vec::new();
        for document in &shards {
            for entry in &document.resources {
                let Some(payload) = entry.payload.as_deref() else {
                    continue;
                };
                match entry.kind {
                    ResourceKind::Agent => agents.push(agent_payload(payload)?),
                    ResourceKind::Terminal => terminals.push(terminal_payload(payload)?),
                }
            }
        }
        let (agents, interrupted_agents) = RuntimeStoreSnapshot {
            records: agents,
            ..RuntimeStoreSnapshot::default()
        }
        .reconcile_after_daemon_restart();
        let (terminals, interrupted_terminals) = TerminalStoreSnapshot {
            records: terminals,
            ..TerminalStoreSnapshot::default()
        }
        .reconcile_after_daemon_restart()
        .map_err(|_| ResourceError::Corrupt)?;
        self.consume_retained(&shards)?;
        Ok(HydratedState {
            agents,
            terminals,
            interrupted: interrupted_agents + interrupted_terminals,
            migration,
        })
    }

    /// Remove the shards of retired generations whose records nothing retains any
    /// more.
    ///
    /// `retained` is the caller's live truth — the resource ids its coordinators
    /// still keep — so history disappears only once its owner *and* the active
    /// generation have both stopped holding it. Returns how many shards were
    /// collected.
    ///
    /// # Errors
    /// Returns a store failure. A shard that is not collectable is left alone
    /// rather than refused.
    pub fn collect_retired(&self, retained: &BTreeSet<String>) -> Result<usize, ResourceFailure> {
        let allocator = self.allocator.load()?.to_document();
        let mut collected = 0;
        for document in self.retained()? {
            if document.owner == self.owner
                || retired_collectable(&document, &allocator).is_err()
                || document
                    .resources
                    .iter()
                    .any(|entry| retained.contains(&entry.resource.terminal_id.as_str()))
            {
                continue;
            }
            self.archive_collect(document.owner)?;
            collected += 1;
        }
        Ok(collected)
    }

    /// Run one bounded collection pass over the operation ledger and the shards
    /// of generations that are no longer live.
    ///
    /// `retained` is the caller's live truth — the resource ids its coordinators
    /// still keep — so nothing is collected while somebody still answers for it.
    ///
    /// # Errors
    /// Returns a store failure. A record or a shard that is not safe to collect is
    /// left alone rather than forced.
    pub fn collect(
        &self,
        retained: &BTreeSet<String>,
        limits: &RetentionLimits,
    ) -> Result<(GcReport, usize), ResourceFailure> {
        let ledger = collect_garbage(&self.allocator, limits, self.clock.as_ref())?;
        let shards = self.collect_retired(retained)?;
        Ok((ledger, shards))
    }

    /// L1: every record that holds capacity has a durable claim first.
    fn claim(&self, projections: &[&RuntimeProjection]) -> Result<(), ResourceFailure> {
        let policy = self.allocator.policy();
        let owner = self.owner;
        let now = self.clock.now();
        self.allocator.update(|document| {
            for projection in projections {
                claim_one(document, projection, owner, policy, now)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// L2/L4: the owner's shard, which only this process writes.
    fn project(
        &self,
        kind: ResourceKind,
        projections: &[&RuntimeProjection],
    ) -> Result<(), ResourceFailure> {
        self.shard.update(|document| {
            for projection in projections {
                project_one(document, projection)?;
            }
            forget_absent(document, kind, projections);
            Ok(())
        })?;
        Ok(())
    }

    /// E2/E3: apply this generation's own published events and sweep the outbox.
    ///
    /// Only an active generation consumes: a draining owner publishes and waits
    /// for the active consumer, which is what keeps the allocator single writer.
    fn drain(&self) -> Result<usize, ResourceFailure> {
        if self.role != GenerationRole::Active {
            return Ok(0);
        }
        let document = self.shard.load()?.to_document();
        let report = ActiveConsumer::new(&self.allocator).consume(&document)?;
        let consumed = self.allocator.load()?.to_document();
        self.shard
            .update(|document| Ok(document.reclaim(&consumed)))?;
        Ok(report.applied)
    }

    /// Release the claims of a retired generation's records this owner's truth
    /// reports as terminated. Nothing else about a foreign record is touched.
    fn release_retired(
        &self,
        projections: &[&RuntimeProjection],
    ) -> Result<usize, ResourceFailure> {
        let terminated: Vec<&TerminalRef> = projections
            .iter()
            .filter(|projection| !projection.state.holds_capacity())
            .map(|projection| &projection.resource)
            .collect();
        if terminated.is_empty() {
            return Ok(0);
        }
        let owner = self.owner;
        let (released, _) = self
            .allocator
            .update(|document| Ok(release_each(document, owner, &terminated)))?;
        Ok(released)
    }

    /// Apply the events every retained shard published, so a crashed owner's exit
    /// still releases its capacity exactly once.
    fn consume_retained(&self, shards: &[ShardDocument]) -> Result<(), ResourceFailure> {
        if self.role != GenerationRole::Active {
            return Ok(());
        }
        let consumer = ActiveConsumer::new(&self.allocator);
        for document in shards {
            consumer.consume(document)?;
        }
        Ok(())
    }

    /// Every retained shard, refused as a whole when one document is not one this
    /// build understands.
    fn retained(&self) -> Result<Vec<ShardDocument>, ResourceFailure> {
        let mut documents = Vec::new();
        for raw in self.documents()? {
            let document: ShardDocument =
                serde_json::from_str(&raw).map_err(|_| ResourceError::Corrupt)?;
            document.validate()?;
            documents.push(document);
        }
        documents.sort_by_key(|document| document.owner.as_str());
        Ok(documents)
    }

    /// Adopt the legacy whole-snapshot stores, once.
    ///
    /// See [`migrate_legacy`] for the write order and what each crash boundary
    /// converges to.
    fn migrate_legacy(&self) -> Result<Option<MigrationReport>, ResourceFailure> {
        let legacy = self.legacy()?;
        if legacy.is_empty() {
            return Ok(None);
        }
        let plan = migrate_legacy(&legacy)?;
        let mut marker = MigrationMarker {
            schema: MIGRATION_SCHEMA.to_owned(),
            generations: Vec::new(),
            adopted: 0,
            unknown: plan.unknown.len(),
        };
        for (owner, resources) in plan.shards {
            let shard = OwnerShard::new(self.shard_file(owner)?, owner);
            let adopted = adopt_shard(&shard, &resources)?;
            marker.generations.push(owner.as_str());
            marker.adopted += adopted;
        }
        self.seal_legacy(&serde_json::to_string(&marker).unwrap_or_default())?;
        Ok(Some(MigrationReport {
            marker,
            unknown: plan.unknown,
        }))
    }

    fn documents(&self) -> io::Result<Vec<String>> {
        self.archive().documents()
    }

    fn legacy(&self) -> io::Result<LegacySnapshots> {
        self.archive().legacy()
    }

    fn shard_file(&self, owner: DaemonGeneration) -> io::Result<Box<dyn CasFile + Send>> {
        self.archive().shard(owner)
    }

    fn seal_legacy(&self, marker: &str) -> io::Result<()> {
        self.archive().seal_legacy(marker)
    }

    fn archive_collect(&self, owner: DaemonGeneration) -> io::Result<()> {
        self.archive().collect(owner)
    }

    fn archive(&self) -> &(dyn ShardArchive + Send) {
        self.archive.as_ref()
    }
}

/// Everything a fresh process needs to hydrate its coordinators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydratedState {
    pub agents: RuntimeStoreSnapshot,
    pub terminals: TerminalStoreSnapshot,
    /// Records the reconcile fenced as `identity_unknown`.
    pub interrupted: usize,
    /// The migration this hydrate performed, when it was the first on this data
    /// directory.
    pub migration: Option<MigrationReport>,
}

/// The shard bodies a legacy migration would write, and what it could not prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    /// One entry per legacy generation, in generation order.
    pub shards: Vec<(DaemonGeneration, Vec<ShardResource>)>,
    pub unknown: Vec<UnknownRecord>,
}

/// The five durable documents this module reads back, each decoded by its own
/// concrete function.
///
/// They are deliberately not one generic helper: a generic would be compiled once
/// per type in every crate that calls it, and an instantiation nothing exercises
/// would read as untested code without describing any untested behaviour.
fn agent_payload(payload: &str) -> Result<DurableRuntimeRecord, ResourceError> {
    serde_json::from_str(payload).map_err(|_| ResourceError::Corrupt)
}

fn terminal_payload(payload: &str) -> Result<DurableTerminalRecord, ResourceError> {
    serde_json::from_str(payload).map_err(|_| ResourceError::Corrupt)
}

fn shard_document(raw: &str) -> Result<ShardDocument, ResourceError> {
    serde_json::from_str(raw).map_err(|_| ResourceError::Corrupt)
}

fn legacy_agents(raw: &str) -> Result<RuntimeStoreSnapshot, ResourceError> {
    let snapshot: RuntimeStoreSnapshot =
        serde_json::from_str(raw).map_err(|_| ResourceError::Corrupt)?;
    snapshot
        .validate_schema()
        .map_err(|_| ResourceError::UnknownSchema)?;
    Ok(snapshot)
}

fn legacy_terminals(raw: &str) -> Result<TerminalStoreSnapshot, ResourceError> {
    let snapshot: TerminalStoreSnapshot =
        serde_json::from_str(raw).map_err(|_| ResourceError::Corrupt)?;
    if snapshot.schema_version != TerminalStoreSnapshot::SCHEMA_VERSION {
        return Err(ResourceError::UnknownSchema);
    }
    Ok(snapshot)
}

/// How much live runtime each pool holds across every retained generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LiveCensus {
    pub agents: usize,
    pub terminals: usize,
}

impl LiveCensus {
    fn add(&mut self, kind: ResourceKind) {
        match kind {
            ResourceKind::Agent => self.agents += 1,
            ResourceKind::Terminal => self.terminals += 1,
        }
    }
}

/// Count the live runtime this data directory holds, writing nothing.
///
/// A lifecycle verb that is about to refuse a transition must not reconcile,
/// migrate, or collect the state it is refusing to destroy, so this reads and
/// nothing else. Legacy stores that have not been migrated yet are counted from
/// their own states: they describe PTYs a cold transition would still destroy.
///
/// # Errors
/// Returns [`ResourceError::Corrupt`] or [`ResourceError::UnknownSchema`] for
/// durable state this build cannot read exactly — never "nothing is live" — or the
/// store's read error.
pub fn census(archive: &dyn ShardArchive) -> Result<LiveCensus, ResourceFailure> {
    let mut census = LiveCensus::default();
    for raw in archive.documents()? {
        let document = shard_document(&raw)?;
        document.validate()?;
        for entry in document
            .resources
            .iter()
            .filter(|entry| entry.state.is_live())
        {
            census.add(entry.kind);
        }
    }
    let legacy = archive.legacy()?;
    let agents = legacy.agents.as_deref().map(legacy_agents).transpose()?;
    let terminals = legacy
        .terminals
        .as_deref()
        .map(legacy_terminals)
        .transpose()?;
    census.agents += agents.as_ref().map_or(0, live_agents);
    census.terminals += terminals.as_ref().map_or(0, live_terminals);
    Ok(census)
}

fn live_agents(snapshot: &RuntimeStoreSnapshot) -> usize {
    snapshot
        .records
        .iter()
        .filter(|record| owns_child(record.state))
        .count()
}

fn live_terminals(snapshot: &TerminalStoreSnapshot) -> usize {
    snapshot
        .records
        .iter()
        .filter(|record| owns_child(record.state))
        .count()
}

/// Whether this shipping state means a PTY master is held right now. A record
/// waiting to be reconciled is not: its owner is already gone.
fn owns_child(state: TerminalRuntimeState) -> bool {
    matches!(
        state,
        TerminalRuntimeState::Reserved | TerminalRuntimeState::Running
    )
}

/// M1 for one legacy generation: its adopted document, created only when the
/// shard is absent. A shard that already exists was adopted by an earlier pass,
/// and re-adopting it could only overwrite newer state.
fn adopt_shard(shard: &OwnerShard, resources: &[ShardResource]) -> Result<usize, ResourceFailure> {
    shard
        .update(|document| Ok(adopt_into(document, resources)))
        .map(|(adopted, _)| adopted)
}

/// Fill an empty shard with the adopted resources, and leave an existing one
/// exactly as it is.
fn adopt_into(document: &mut ShardDocument, resources: &[ShardResource]) -> usize {
    if document.revision > 0 {
        return 0;
    }
    document.resources = resources.to_vec();
    document.resources.len()
}

/// Release every claim a retired generation still holds for a resource this
/// owner's truth reports as terminated. Returns how many were released.
fn release_each(
    document: &mut AllocatorDocument,
    owner: DaemonGeneration,
    terminated: &[&TerminalRef],
) -> usize {
    terminated
        .iter()
        .filter(|resource| document.release_unowned(owner, resource).is_ok())
        .count()
}

/// L1 for one record: the shared claim, and the producer's durable answer.
///
/// A record that holds capacity must have a claim before its shard reservation
/// exists, and the seal that follows it is applied only while the operation has no
/// final yet: a recorded final is the producer's answer and is never rewritten.
fn claim_one(
    document: &mut AllocatorDocument,
    projection: &RuntimeProjection,
    owner: DaemonGeneration,
    policy: CapacityPolicy,
    now: u64,
) -> Result<(), ResourceError> {
    if projection.state.holds_capacity() {
        document.reserve(
            &projection.operation,
            &projection.digest,
            projection.kind,
            owner,
            &projection.resource,
            policy,
        )?;
    }
    let Some(record) = document.operation(&projection.operation) else {
        // The ledger already collected this operation. Its record is history now,
        // and history is not re-admitted.
        return Ok(());
    };
    if record.outcome.is_final() {
        return Ok(());
    }
    match projection.state {
        ProjectedState::Running(_) | ProjectedState::Exited => {
            document.mark_spawned(&projection.operation, now)
        }
        ProjectedState::SpawnFailed => {
            document.mark_failed(&projection.operation, LaunchFailure::Spawn, now)
        }
        // A child may exist and nothing can prove it does not, so the answer is a
        // durable ambiguous final that never releases capacity.
        ProjectedState::Unproven => document.mark_ambiguous(&projection.operation, now),
        ProjectedState::Reserved => Ok(()),
    }
}

/// L2/L4 for one record: the owner's reservation, its state, and its payload, all
/// in the one compare-and-swap this shard update commits.
fn project_one(
    document: &mut ShardDocument,
    projection: &RuntimeProjection,
) -> Result<(), ResourceError> {
    document.reserve(
        &projection.operation,
        &projection.digest,
        projection.kind,
        &projection.resource,
    )?;
    match &projection.state {
        ProjectedState::Reserved => Ok(()),
        ProjectedState::Running(identity) => document.record_spawn(&projection.resource, identity),
        // The shipping stores keep no exit status, so none is invented here; the
        // exit itself is what releases the capacity.
        ProjectedState::Exited | ProjectedState::SpawnFailed => {
            match document.commit_exit(&projection.resource, None) {
                // A record that never reached a live state cannot "exit"; it is
                // already terminal in the shard and needs no second transition.
                Err(ResourceError::WrongState) => Ok(()),
                other => other,
            }
        }
        ProjectedState::Unproven => document.mark_ownership_unknown(&projection.resource),
    }?;
    document.set_payload(&projection.resource, &projection.payload)
}

/// Drop the shard resources of this kind the owner no longer retains.
///
/// A resource that is still live but has left the owner's truth is not dropped:
/// forgetting it would hide a child nothing would reap, so it becomes
/// `OwnershipUnknown` and keeps its capacity instead.
fn forget_absent(
    document: &mut ShardDocument,
    kind: ResourceKind,
    projections: &[&RuntimeProjection],
) {
    let present: BTreeSet<String> = projections
        .iter()
        .map(|projection| projection.resource.terminal_id.as_str())
        .collect();
    let absent: Vec<TerminalRef> = document
        .resources
        .iter()
        .filter(|entry| {
            entry.kind == kind && !present.contains(&entry.resource.terminal_id.as_str())
        })
        .map(|entry| entry.resource.clone())
        .collect();
    for resource in absent {
        if document.forget(&resource).is_err() {
            // `forget` refuses exactly one case — a live resource — and the safe
            // answer for it is the unprovable state, never a silent drop.
            let _ = document.mark_ownership_unknown(&resource);
        }
    }
}

/// Plan the adoption of the legacy whole-snapshot stores.
///
/// The write order the caller then performs, and what each crash boundary leaves:
///
/// ```text
/// M1  shard CAS per legacy generation   the adopted document, created only when absent
/// M2  marker + legacy retirement        the one-way step
/// ```
///
/// | crash boundary | durable state | next pass |
/// |---|---|---|
/// | before M1 | legacy stores only | adopts from the same bytes, deterministically |
/// | M1..M2 | some shards adopted | skips the shards that exist, adopts the rest |
/// | after M2 | shards only | the legacy stores are gone, so nothing is adopted again |
///
/// No capacity claim is taken: a legacy record's identity is a token an older build
/// wrote, so it can never be OS-verified and every adopted record is
/// `OwnershipUnknown`. Such a record is never admitted or replayed, so giving it a
/// ledger entry would make a synthetic placeholder look like a producer operation.
///
/// No boundary can spawn or kill anything: adoption creates records, never
/// children, and a record it cannot prove is adopted as `OwnershipUnknown`.
/// Downgrading afterwards is therefore a *cold* transition, not a seamless one —
/// an older build finds no legacy store, starts with no runtime, and the marker is
/// the durable evidence of why.
///
/// # Errors
/// Returns [`ResourceError::UnknownSchema`] for a legacy schema this build does
/// not understand and [`ResourceError::Corrupt`] for bytes that are not a legacy
/// document. Neither is guessed past: a store that cannot be read exactly is not
/// migrated at all.
pub fn migrate_legacy(legacy: &LegacySnapshots) -> Result<MigrationPlan, ResourceError> {
    let mut records: Vec<(LegacyRuntimeRecord, String)> = Vec::new();
    if let Some(raw) = legacy.agents.as_deref() {
        let snapshot = legacy_agents(raw)?;
        for record in &snapshot.records {
            let payload = serde_json::to_string(record).unwrap_or_default();
            records.push((legacy_agent(record), payload));
        }
    }
    if let Some(raw) = legacy.terminals.as_deref() {
        let snapshot = legacy_terminals(raw)?;
        for record in &snapshot.records {
            let payload = serde_json::to_string(record).unwrap_or_default();
            records.push((legacy_terminal(record), payload));
        }
    }
    let mut grouped: BTreeMap<DaemonGeneration, Vec<(LegacyRuntimeRecord, String)>> =
        BTreeMap::new();
    for entry in records {
        grouped
            .entry(entry.0.resource.daemon_generation)
            .or_default()
            .push(entry);
    }
    let mut plan = MigrationPlan {
        shards: Vec::new(),
        unknown: Vec::new(),
    };
    for (owner, entries) in grouped {
        let legacy: Vec<LegacyRuntimeRecord> =
            entries.iter().map(|(record, _)| record.clone()).collect();
        let report = adopt_legacy(owner, &legacy);
        let mut resources = report.shard.resources;
        for entry in &mut resources {
            entry.payload = entries
                .iter()
                .find(|(record, _)| record.resource.terminal_id == entry.resource.terminal_id)
                .map(|(_, payload)| payload.clone());
        }
        plan.shards.push((owner, resources));
        plan.unknown.extend(report.unknown);
    }
    Ok(plan)
}

/// One legacy Agent record in the migration vocabulary.
///
/// The identity is marked explicitly unverifiable: production wrote a fixed token
/// which proves nothing, and pretending otherwise is what would let a reused PID
/// be adopted as somebody's child.
fn legacy_agent(record: &DurableRuntimeRecord) -> LegacyRuntimeRecord {
    LegacyRuntimeRecord {
        resource: record.runtime.terminal.clone(),
        kind: ResourceKind::Agent,
        operation: Some(record.operation.operation_id),
        digest: Some(digest_of(
            ResourceKind::Agent,
            record.semantic_key.as_deref().unwrap_or_default(),
        )),
        process: record.process.as_ref().map(legacy_identity),
        live: unterminated(record.state),
    }
}

/// One legacy generic terminal record in the migration vocabulary.
fn legacy_terminal(record: &DurableTerminalRecord) -> LegacyRuntimeRecord {
    LegacyRuntimeRecord {
        resource: record.terminal.clone(),
        kind: ResourceKind::Terminal,
        operation: Some(record.operation.operation_id),
        digest: Some(digest_of(
            ResourceKind::Terminal,
            record.launch_digest.as_deref().unwrap_or_default(),
        )),
        process: record.process.as_ref().map(legacy_identity),
        live: unterminated(record.state),
    }
}

fn legacy_identity(process: &ProcessIdentity) -> ChildIdentity {
    ChildIdentity::unverifiable(process.pid, process.start_identity.clone())
}

/// Whether the legacy store still considered a record to hold a child. It is the
/// same set the shipping coordinators count as an occupied slot, so migration does
/// not change what "live" means.
fn unterminated(state: TerminalRuntimeState) -> bool {
    matches!(
        state,
        TerminalRuntimeState::Reserved
            | TerminalRuntimeState::Running
            | TerminalRuntimeState::ReconcileRequired(_)
    )
}

/// The shipping Agent store, on this generation's shard.
pub struct ShardedAgentStore {
    state: ShardedRuntimeState,
}

impl ShardedAgentStore {
    /// Bind the Agent store to one generation's durable state.
    #[must_use]
    pub fn new(state: ShardedRuntimeState) -> Self {
        Self { state }
    }

    /// The durable state, for hydrate and collection passes.
    #[must_use]
    pub fn state(&self) -> &ShardedRuntimeState {
        &self.state
    }
}

impl RuntimeStore for ShardedAgentStore {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        let projections: Vec<RuntimeProjection> = snapshot
            .records
            .iter()
            .map(|record| project_agent(record, self.state.identity()))
            .collect();
        self.state
            .commit(ResourceKind::Agent, &projections)
            .map(|_| ())
            .map_err(|_| ())
    }
}

/// The shipping generic terminal store, on this generation's shard.
pub struct ShardedTerminalStore {
    state: ShardedRuntimeState,
}

impl ShardedTerminalStore {
    /// Bind the terminal store to one generation's durable state.
    #[must_use]
    pub fn new(state: ShardedRuntimeState) -> Self {
        Self { state }
    }

    /// The durable state, for hydrate and collection passes.
    #[must_use]
    pub fn state(&self) -> &ShardedRuntimeState {
        &self.state
    }
}

impl TerminalStore for ShardedTerminalStore {
    fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), ()> {
        let projections: Vec<RuntimeProjection> = snapshot
            .records
            .iter()
            .map(|record| project_terminal(record, self.state.identity()))
            .collect();
        self.state
            .commit(ResourceKind::Terminal, &projections)
            .map(|_| ())
            .map_err(|_| ())
    }
}

#[cfg(test)]
mod tests;

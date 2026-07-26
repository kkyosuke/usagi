//! The global resource allocator: one durable document every retained
//! generation compare-and-swaps.
//!
//! It answers two questions no single process can answer alone:
//!
//! * **capacity** — how many resources of each kind exist across *all* active
//!   and draining generations. Each kind has its own pool and the pools are never
//!   implicitly summed, so the Agent limit and the generic-terminal limit keep the
//!   meaning they have today.
//! * **producer operations** — what a given producer [`OperationId`] already
//!   decided. The same id with the same canonical intent replays the recorded
//!   answer; the same id with a different intent is an idempotency conflict and
//!   changes nothing.
//!
//! A claim is created *before* the owner reserves and *before* anything is
//! spawned, so the authority always leads the effect ([`super::launch`]).
//! Capacity is released exactly once, and only against evidence: a definite
//! failure, or an exit event consumed from the owning generation's outbox
//! ([`super::drain`]).

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use crate::usecase::resources::{CasDocument, CasFile, CasStore, ResourceError};

/// The only allocator schema this build understands.
pub const ALLOCATOR_SCHEMA: &str = "usagi-resource-allocator-v1";

/// A resource kind, which is also its capacity pool. Agent runtimes and generic
/// terminals are counted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Agent,
    Terminal,
}

impl ResourceKind {
    /// The pool name this kind reserves from.
    #[must_use]
    pub fn pool(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Terminal => "terminal",
        }
    }
}

/// Per-pool concurrency limits shared by every retained generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityPolicy {
    agent: usize,
    terminal: usize,
}

impl CapacityPolicy {
    /// Build a policy from the existing per-kind limits.
    #[must_use]
    pub fn new(agent: usize, terminal: usize) -> Self {
        Self { agent, terminal }
    }

    /// The limit for one pool. Pools are independent by construction: there is no
    /// accessor for a combined total.
    #[must_use]
    pub fn limit(&self, kind: ResourceKind) -> usize {
        match kind {
            ResourceKind::Agent => self.agent,
            ResourceKind::Terminal => self.terminal,
        }
    }
}

/// A capacity claim's durable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimState {
    /// Capacity is held for a launch that has not been proved spawned.
    Reserved,
    /// Capacity is held by a resource that was proved spawned.
    Live,
    /// Capacity was released exactly once against definite evidence.
    Released,
}

/// One capacity claim, keyed by the resource it holds capacity for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceClaim {
    pub resource: TerminalRef,
    pub kind: ResourceKind,
    pub owner: DaemonGeneration,
    pub operation: OperationId,
    pub digest: String,
    pub state: ClaimState,
    pub revision: u64,
}

/// Why a launch failed definitely, meaning no child exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchFailure {
    /// The owner could not make its reservation durable.
    Reservation,
    /// The spawn itself failed and left no process.
    Spawn,
}

/// The durable answer for one producer operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcome {
    /// Capacity is claimed; whether a child exists is not decided yet.
    Reserved,
    /// A child was proved spawned. Durable final.
    Spawned,
    /// No child exists. Durable final.
    Failed(LaunchFailure),
    /// A child may or may not exist and the platform cannot say. Durable final
    /// that never releases capacity and is never collected.
    Ambiguous,
}

impl OperationOutcome {
    /// Whether this outcome is a durable final that must be replayed verbatim.
    #[must_use]
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Reserved)
    }

    /// Whether this outcome may ever be collected by retention.
    #[must_use]
    pub fn is_collectable(self) -> bool {
        matches!(self, Self::Spawned | Self::Failed(_))
    }
}

/// One producer operation's full outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub operation: OperationId,
    /// Canonical intent digest. A different digest under the same id conflicts.
    pub digest: String,
    pub kind: ResourceKind,
    pub owner: DaemonGeneration,
    pub resource: TerminalRef,
    pub outcome: OperationOutcome,
    /// Logical time the final was committed; `None` while not final. Retention
    /// ages a record from this, never from an incoming timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_at: Option<u64>,
    pub revision: u64,
}

/// The compact, non-reusable replacement for an evicted full outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryClass {
    Spawned,
    Failed,
}

/// A compacted operation. It keeps only what a retry needs to be refused
/// safely: the id, the canonical digest, and when it expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationTombstone {
    pub operation: OperationId,
    pub digest: String,
    pub class: ExpiryClass,
    /// Logical time after which the exact tombstone itself may be compacted.
    pub cutoff: u64,
}

/// One owner-published event that the active consumer has applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumedEvent {
    pub resource: TerminalRef,
    pub owner: DaemonGeneration,
    /// The highest owner event revision applied for this resource.
    pub event_revision: u64,
}

/// Whether an admission may proceed or must replay a recorded answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Nothing is recorded for this operation: proceed to reserve.
    Fresh,
    /// The operation is already recorded: return exactly this answer.
    Replay {
        resource: TerminalRef,
        outcome: OperationOutcome,
        revision: u64,
    },
}

/// What consuming an owner event did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeOutcome {
    /// This call applied the event: capacity moved to released exactly here.
    Applied,
    /// The event (or a newer one for the same resource) was already applied.
    AlreadyConsumed,
}

/// The whole allocator document. `revision` covers the document, so every commit
/// is a compare-and-swap against one monotonic counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocatorDocument {
    pub schema: String,
    pub revision: u64,
    pub claims: Vec<ResourceClaim>,
    pub operations: Vec<OperationRecord>,
    pub tombstones: Vec<OperationTombstone>,
    pub consumed: Vec<ConsumedEvent>,
    /// Every operation id at or below this watermark is permanently expired. It
    /// only advances from the server's own accepted history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<OperationId>,
}

impl Default for AllocatorDocument {
    fn default() -> Self {
        Self {
            schema: ALLOCATOR_SCHEMA.to_owned(),
            revision: 0,
            claims: Vec::new(),
            operations: Vec::new(),
            tombstones: Vec::new(),
            consumed: Vec::new(),
            watermark: None,
        }
    }
}

impl CasDocument for AllocatorDocument {
    fn revision(&self) -> u64 {
        self.revision
    }

    fn bump(&mut self) {
        self.revision += 1;
    }

    fn validate(&self) -> Result<(), ResourceError> {
        if self.schema != ALLOCATOR_SCHEMA {
            return Err(ResourceError::UnknownSchema);
        }
        let mut resources = std::collections::BTreeSet::new();
        for claim in &self.claims {
            if !resources.insert(claim.resource.terminal_id.as_str())
                || claim.resource.daemon_generation != claim.owner
                || self.operation(&claim.operation).is_none()
            {
                return Err(ResourceError::Corrupt);
            }
        }
        let mut operations = std::collections::BTreeSet::new();
        for record in &self.operations {
            if !operations.insert(record.operation.as_str())
                || record.resource.daemon_generation != record.owner
                || record.outcome.is_final() != record.sealed_at.is_some()
            {
                return Err(ResourceError::Corrupt);
            }
            if self
                .watermark
                .as_ref()
                .is_some_and(|watermark| precedes_or_equals(&record.operation, watermark))
            {
                return Err(ResourceError::Corrupt);
            }
        }
        for tombstone in &self.tombstones {
            if !operations.insert(tombstone.operation.as_str()) {
                return Err(ResourceError::Corrupt);
            }
        }
        for event in &self.consumed {
            if self.claim(&event.resource).is_none() {
                return Err(ResourceError::Corrupt);
            }
        }
        Ok(())
    }
}

impl AllocatorDocument {
    /// The claim holding capacity for `resource`, if any.
    #[must_use]
    pub fn claim(&self, resource: &TerminalRef) -> Option<&ResourceClaim> {
        self.claims
            .iter()
            .find(|claim| claim.resource.terminal_id == resource.terminal_id)
    }

    /// The full outcome recorded for `operation`, if it is still retained.
    #[must_use]
    pub fn operation(&self, operation: &OperationId) -> Option<&OperationRecord> {
        self.operations
            .iter()
            .find(|record| &record.operation == operation)
    }

    /// The compact tombstone for `operation`, if its full outcome was evicted.
    #[must_use]
    pub fn tombstone(&self, operation: &OperationId) -> Option<&OperationTombstone> {
        self.tombstones
            .iter()
            .find(|tombstone| &tombstone.operation == operation)
    }

    /// How much of one pool is held. Released claims hold nothing.
    #[must_use]
    pub fn pool_used(&self, kind: ResourceKind) -> usize {
        self.claims
            .iter()
            .filter(|claim| claim.kind == kind && claim.state != ClaimState::Released)
            .count()
    }

    /// How many claims one generation still holds, which gates its collection.
    #[must_use]
    pub fn owner_claims(&self, owner: DaemonGeneration) -> usize {
        self.claims
            .iter()
            .filter(|claim| claim.owner == owner && claim.state != ClaimState::Released)
            .count()
    }

    /// The highest owner event revision already applied for `resource`.
    #[must_use]
    pub fn consumed_revision(&self, resource: &TerminalRef) -> Option<u64> {
        self.consumed
            .iter()
            .find(|event| event.resource.terminal_id == resource.terminal_id)
            .map(|event| event.event_revision)
    }

    /// Decide whether a producer operation may take fresh capacity.
    ///
    /// This is pure and effect free: every refusal leaves the document, the
    /// existing terminals, and the pools untouched.
    ///
    /// # Errors
    /// Returns [`ResourceError::OperationExpired`] for a compacted or
    /// below-watermark id, [`ResourceError::OperationConflict`] for the same id
    /// with a different canonical intent, or [`ResourceError::CapacityExhausted`]
    /// when the kind's own pool is full.
    pub fn admit(
        &self,
        operation: &OperationId,
        digest: &str,
        kind: ResourceKind,
        policy: CapacityPolicy,
    ) -> Result<Admission, ResourceError> {
        if self.is_expired(operation) {
            return Err(ResourceError::OperationExpired);
        }
        if let Some(record) = self.operation(operation) {
            if record.digest != digest {
                return Err(ResourceError::OperationConflict);
            }
            return Ok(Admission::Replay {
                resource: record.resource.clone(),
                outcome: record.outcome,
                revision: record.revision,
            });
        }
        if self.pool_used(kind) >= policy.limit(kind) {
            return Err(ResourceError::CapacityExhausted);
        }
        Ok(Admission::Fresh)
    }

    /// Whether this id can never be admitted again.
    #[must_use]
    pub fn is_expired(&self, operation: &OperationId) -> bool {
        self.tombstone(operation).is_some()
            || self
                .watermark
                .as_ref()
                .is_some_and(|watermark| precedes_or_equals(operation, watermark))
    }

    /// Take the global claim for a launch (the first durable write of a launch).
    ///
    /// # Errors
    /// Returns [`admit`](Self::admit)'s refusal, or
    /// [`ResourceError::DuplicateResource`] when the proposed resource id is
    /// already claimed by another operation.
    pub fn reserve(
        &mut self,
        operation: &OperationId,
        digest: &str,
        kind: ResourceKind,
        owner: DaemonGeneration,
        resource: &TerminalRef,
        policy: CapacityPolicy,
    ) -> Result<Admission, ResourceError> {
        let admission = self.admit(operation, digest, kind, policy)?;
        if admission != Admission::Fresh {
            return Ok(admission);
        }
        if self.claim(resource).is_some() || resource.daemon_generation != owner {
            return Err(ResourceError::DuplicateResource);
        }
        self.operations.push(OperationRecord {
            operation: *operation,
            digest: digest.to_owned(),
            kind,
            owner,
            resource: resource.clone(),
            outcome: OperationOutcome::Reserved,
            sealed_at: None,
            revision: 1,
        });
        self.claims.push(ResourceClaim {
            resource: resource.clone(),
            kind,
            owner,
            operation: *operation,
            digest: digest.to_owned(),
            state: ClaimState::Reserved,
            revision: 1,
        });
        Ok(Admission::Fresh)
    }

    /// Record that a child was proved spawned: the claim becomes live and the
    /// operation reaches its durable final.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownOperation`], or
    /// [`ResourceError::WrongState`] when the recorded final is a different one.
    pub fn mark_spawned(&mut self, operation: &OperationId, now: u64) -> Result<(), ResourceError> {
        self.seal(operation, OperationOutcome::Spawned, now)?;
        let record = self.expect_operation(operation)?.clone();
        if let Some(claim) = self.claim_mut(&record.resource)
            && claim.state == ClaimState::Reserved
        {
            claim.state = ClaimState::Live;
            claim.revision += 1;
        }
        Ok(())
    }

    /// Record a definite failure: no child exists, so capacity is released here
    /// exactly once.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownOperation`] or
    /// [`ResourceError::WrongState`].
    pub fn mark_failed(
        &mut self,
        operation: &OperationId,
        reason: LaunchFailure,
        now: u64,
    ) -> Result<(), ResourceError> {
        self.seal(operation, OperationOutcome::Failed(reason), now)?;
        let record = self.expect_operation(operation)?.clone();
        self.release(&record.resource);
        Ok(())
    }

    /// Record an ambiguous spawn. It is a durable final that is replayed
    /// verbatim, and it never releases capacity: a child may exist.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownOperation`] or
    /// [`ResourceError::WrongState`].
    pub fn mark_ambiguous(
        &mut self,
        operation: &OperationId,
        now: u64,
    ) -> Result<(), ResourceError> {
        self.seal(operation, OperationOutcome::Ambiguous, now)
    }

    /// Apply one owner-published progress event (output, command completion).
    /// It advances the consumed revision without touching capacity.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`] or
    /// [`ResourceError::WrongOwner`].
    pub fn consume_progress(
        &mut self,
        owner: DaemonGeneration,
        resource: &TerminalRef,
        event_revision: u64,
    ) -> Result<ConsumeOutcome, ResourceError> {
        self.consume_event(owner, resource, event_revision, false)
    }

    /// Apply one owner-published terminal event (an exit). This is the single
    /// place a claim becomes released after a child exits, and it happens exactly
    /// once however often the event is redelivered.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`] or
    /// [`ResourceError::WrongOwner`].
    pub fn consume_exit(
        &mut self,
        owner: DaemonGeneration,
        resource: &TerminalRef,
        event_revision: u64,
    ) -> Result<ConsumeOutcome, ResourceError> {
        self.consume_event(owner, resource, event_revision, true)
    }

    /// Release the capacity of a resource whose child is *proved gone* while its
    /// owner is *proved dead*.
    ///
    /// Every other release is an event the owner published. A dead owner never
    /// publishes again, so without this its capacity would be held for the
    /// lifetime of the data directory. The two proofs are the caller's to
    /// establish — which is why `owner` is a parameter and is not read out of the
    /// claim — and both are required: a live owner still owns its own exits, and
    /// a child that is not proved gone may still be running.
    ///
    /// It is idempotent: a claim already released stays released exactly once.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`] when nothing holds capacity for
    /// this resource, or [`ResourceError::WrongOwner`] when the claim belongs to
    /// another generation.
    pub fn release_gone(
        &mut self,
        owner: DaemonGeneration,
        resource: &TerminalRef,
    ) -> Result<(), ResourceError> {
        let claim = self.claim(resource).ok_or(ResourceError::UnknownResource)?;
        if claim.owner != owner {
            return Err(ResourceError::WrongOwner);
        }
        self.release(resource);
        Ok(())
    }

    /// Apply one owner-published event for a resource: idempotent, ordered, and
    /// owner fenced. Only the active consumer calls this.
    fn consume_event(
        &mut self,
        owner: DaemonGeneration,
        resource: &TerminalRef,
        event_revision: u64,
        terminal: bool,
    ) -> Result<ConsumeOutcome, ResourceError> {
        let claim = self
            .claim(resource)
            .ok_or(ResourceError::UnknownResource)?
            .clone();
        if claim.owner != owner {
            return Err(ResourceError::WrongOwner);
        }
        if self
            .consumed_revision(resource)
            .is_some_and(|applied| applied >= event_revision)
        {
            return Ok(ConsumeOutcome::AlreadyConsumed);
        }
        match self
            .consumed
            .iter_mut()
            .find(|event| event.resource.terminal_id == resource.terminal_id)
        {
            Some(existing) => existing.event_revision = event_revision,
            None => self.consumed.push(ConsumedEvent {
                resource: resource.clone(),
                owner,
                event_revision,
            }),
        }
        if terminal {
            self.release(resource);
        }
        Ok(ConsumeOutcome::Applied)
    }

    fn seal(
        &mut self,
        operation: &OperationId,
        outcome: OperationOutcome,
        now: u64,
    ) -> Result<(), ResourceError> {
        let record = self.expect_operation_mut(operation)?;
        if record.outcome == outcome {
            return Ok(());
        }
        if record.outcome.is_final() {
            return Err(ResourceError::WrongState);
        }
        record.outcome = outcome;
        record.sealed_at = Some(now);
        record.revision += 1;
        Ok(())
    }

    fn release(&mut self, resource: &TerminalRef) {
        if let Some(claim) = self.claim_mut(resource)
            && claim.state != ClaimState::Released
        {
            claim.state = ClaimState::Released;
            claim.revision += 1;
        }
    }

    fn claim_mut(&mut self, resource: &TerminalRef) -> Option<&mut ResourceClaim> {
        self.claims
            .iter_mut()
            .find(|claim| claim.resource.terminal_id == resource.terminal_id)
    }

    fn expect_operation(&self, operation: &OperationId) -> Result<&OperationRecord, ResourceError> {
        self.operation(operation)
            .ok_or(ResourceError::UnknownOperation)
    }

    fn expect_operation_mut(
        &mut self,
        operation: &OperationId,
    ) -> Result<&mut OperationRecord, ResourceError> {
        self.operations
            .iter_mut()
            .find(|record| &record.operation == operation)
            .ok_or(ResourceError::UnknownOperation)
    }
}

/// Whether `candidate` is at or below `watermark` in `UUIDv7` order.
///
/// Canonical `UUIDv7` text sorts by its embedded timestamp, so this is a total
/// order over issued ids without parsing them back into time.
#[must_use]
pub fn precedes_or_equals(candidate: &OperationId, watermark: &OperationId) -> bool {
    candidate.as_str() <= watermark.as_str()
}

/// The global allocator over a [`CasFile`].
pub struct ResourceAllocator {
    store: CasStore<AllocatorDocument>,
    policy: CapacityPolicy,
}

impl ResourceAllocator {
    /// Bind an allocator to its durable document and per-pool policy.
    pub fn new(file: impl CasFile + Send + 'static, policy: CapacityPolicy) -> Self {
        Self {
            store: CasStore::new(file),
            policy,
        }
    }

    /// The per-pool policy this allocator enforces.
    #[must_use]
    pub fn policy(&self) -> CapacityPolicy {
        self.policy
    }

    /// The compare-and-swapped store, for callers staging their own transition.
    #[must_use]
    pub fn store(&self) -> &CasStore<AllocatorDocument> {
        &self.store
    }

    /// Read and validate the document; an absent document is an empty allocator.
    ///
    /// # Errors
    /// Returns the store's read error or the document's validation refusal.
    pub fn load(&self) -> Result<super::CasSnapshot<AllocatorDocument>, super::ResourceFailure> {
        self.store.load(AllocatorDocument::default)
    }

    /// Apply `change` under one compare-and-swap.
    ///
    /// # Errors
    /// Returns `change`'s refusal or the store's failure.
    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut AllocatorDocument) -> Result<T, ResourceError>,
    ) -> Result<(T, super::CasSnapshot<AllocatorDocument>), super::ResourceFailure> {
        self.store.update(AllocatorDocument::default, change)
    }
}

#[cfg(test)]
mod tests;

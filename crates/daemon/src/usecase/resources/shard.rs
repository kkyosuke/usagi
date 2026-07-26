//! One owner generation's runtime shard: the state exactly one process writes.
//!
//! Today's `agents.json` / `terminals.json` are whole-snapshot replacements. That
//! is safe while `daemon.lock` guarantees a single daemon, and unsafe the moment a
//! planned rollover keeps a draining owner and a new active owner alive together:
//! both load the same bytes and the last rename wins.
//!
//! A shard removes the race instead of merging it away. Each retained generation
//! owns one document, named after itself, and only its own process writes it:
//!
//! ```text
//! shards/<G1>.json   written by G1 only   ── outbox ──▶ allocator ──▶ consumed by G2
//! shards/<G2>.json   written by G2 only
//! ```
//!
//! A shard therefore never needs a merge. What crosses generations is not state
//! but *events*: the draining owner appends to its own outbox, the active
//! consumer applies them through the global allocator, and the owner reclaims its
//! outbox after observing the consumed revision. The active generation never
//! writes the old shard — [`super::drain::ActiveConsumer`] structurally cannot.
//!
//! A standby only ever [`hydrate`]s, which is read only, and seals what it read.
//! It becomes a writer through [`WriterLease`] after the handoff commits, and only
//! if neither the shard nor the allocator moved under the seal.

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use crate::usecase::resources::allocator::{AllocatorDocument, ResourceKind};
use crate::usecase::resources::identity::ChildIdentity;
use crate::usecase::resources::{
    CasDocument, CasFile, CasSnapshot, CasStore, ResourceError, ResourceFailure,
};

/// The only shard schema this build understands.
pub const SHARD_SCHEMA: &str = "usagi-owner-shard-v1";

/// A resource's runtime state inside its owner's shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    /// Reserved durably; no child spawned yet.
    Reserved,
    /// A child was spawned and its identity is OS verifiable.
    Running,
    /// The child exited and the exit is committed to this shard.
    Exited { status: i32 },
    /// Ownership cannot be proved. Nothing is spawned, signalled, or released
    /// for this record.
    OwnershipUnknown,
}

impl ResourceState {
    /// Whether this state still holds a child this owner is responsible for.
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Reserved | Self::Running)
    }
}

/// One resource owned by this generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardResource {
    pub resource: TerminalRef,
    pub kind: ResourceKind,
    /// The producer operation this resource was launched for.
    pub operation: OperationId,
    /// Canonical intent digest, kept so a replay can be proved identical.
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<ChildIdentity>,
    pub state: ResourceState,
    pub revision: u64,
}

/// An owner-local event that the active generation must apply exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OwnerEvent {
    /// Output was journalled up to this offset.
    Output { offset: u64 },
    /// A terminal command reached its completion.
    CommandCompleted { command: OperationId },
    /// The child exited. This is the only event that releases capacity.
    Exit { status: i32 },
}

impl OwnerEvent {
    /// Whether applying this event releases the resource's capacity claim.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Exit { .. })
    }
}

/// One entry in the owner's outbox, revisioned so redelivery is detectable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEvent {
    pub event_revision: u64,
    pub resource: TerminalRef,
    pub event: OwnerEvent,
}

/// A terminal command this owner accepted and has not completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InFlightCommand {
    pub resource: TerminalRef,
    pub command: OperationId,
}

/// One owner-local durable payload, opaque to the cross-generation contract.
///
/// The contract fields above answer "who owns what, and may it act": that is what
/// another generation reads. A payload is the rest of one resource kind's durable
/// record — the descriptive state only its own owner ever reads back
/// ([`crate::usecase::runtime_shard`] binds the production stores to it). It lives
/// in the same document so an owner commits its records and their meaning in one
/// compare-and-swap, instead of in two objects a crash could split.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardPayload {
    pub kind: ResourceKind,
    pub document: serde_json::Value,
}

/// The whole shard document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardDocument {
    pub schema: String,
    pub owner: DaemonGeneration,
    pub revision: u64,
    pub resources: Vec<ShardResource>,
    pub outbox: Vec<OutboxEvent>,
    pub in_flight: Vec<InFlightCommand>,
    /// Monotonic event revision issued by this owner alone.
    pub event_sequence: u64,
    /// The owner-local payload of each resource kind, at most one per kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payloads: Vec<ShardPayload>,
}

impl ShardDocument {
    /// An empty shard for `owner`.
    #[must_use]
    pub fn empty(owner: DaemonGeneration) -> Self {
        Self {
            schema: SHARD_SCHEMA.to_owned(),
            owner,
            revision: 0,
            resources: Vec::new(),
            outbox: Vec::new(),
            in_flight: Vec::new(),
            event_sequence: 0,
            payloads: Vec::new(),
        }
    }

    /// The owner-local payload of one resource kind, if this shard carries one.
    #[must_use]
    pub fn payload(&self, kind: ResourceKind) -> Option<&serde_json::Value> {
        self.payloads
            .iter()
            .find(|payload| payload.kind == kind)
            .map(|payload| &payload.document)
    }

    /// Replace one resource kind's owner-local payload, leaving the other kind's
    /// payload untouched. Writing the identical payload changes nothing, so a
    /// converged save commits no revision at all.
    pub fn set_payload(&mut self, kind: ResourceKind, document: serde_json::Value) {
        match self
            .payloads
            .iter_mut()
            .find(|payload| payload.kind == kind)
        {
            Some(existing) => existing.document = document,
            None => self.payloads.push(ShardPayload { kind, document }),
        }
    }
}

impl CasDocument for ShardDocument {
    fn revision(&self) -> u64 {
        self.revision
    }

    fn bump(&mut self) {
        self.revision += 1;
    }

    fn validate(&self) -> Result<(), ResourceError> {
        if self.schema != SHARD_SCHEMA {
            return Err(ResourceError::UnknownSchema);
        }
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.resources {
            if !seen.insert(entry.resource.terminal_id.as_str())
                || entry.resource.daemon_generation != self.owner
            {
                return Err(ResourceError::Corrupt);
            }
            let verifiable = entry
                .process
                .as_ref()
                .is_some_and(ChildIdentity::is_verifiable);
            match entry.state {
                // A running child is only ever recorded together with an
                // identity that can be re-observed later.
                ResourceState::Running if !verifiable => return Err(ResourceError::Corrupt),
                ResourceState::Reserved if entry.process.is_some() => {
                    return Err(ResourceError::Corrupt);
                }
                _ => {}
            }
        }
        let mut revisions = std::collections::BTreeSet::new();
        for event in &self.outbox {
            if event.event_revision > self.event_sequence
                || !revisions.insert(event.event_revision)
                || self.resource(&event.resource).is_none()
            {
                return Err(ResourceError::Corrupt);
            }
        }
        for command in &self.in_flight {
            if self.resource(&command.resource).is_none() {
                return Err(ResourceError::Corrupt);
            }
        }
        // Two payloads of one kind would make "the owner's record of this kind"
        // ambiguous, and a reader must never pick one of two answers.
        let mut kinds = std::collections::BTreeSet::new();
        for payload in &self.payloads {
            if !kinds.insert(payload.kind.pool()) {
                return Err(ResourceError::Corrupt);
            }
        }
        Ok(())
    }
}

impl ShardDocument {
    /// The record for `resource`, if this shard owns it.
    #[must_use]
    pub fn resource(&self, resource: &TerminalRef) -> Option<&ShardResource> {
        self.resources
            .iter()
            .find(|entry| entry.resource.terminal_id == resource.terminal_id)
    }

    /// The resources still holding a child.
    #[must_use]
    pub fn live_resources(&self) -> usize {
        self.resources
            .iter()
            .filter(|entry| entry.state.is_live())
            .count()
    }

    /// Reserve a resource for a producer operation. Repeating the identical
    /// reservation is idempotent, so a retry cannot create a second record.
    ///
    /// # Errors
    /// Returns [`ResourceError::ForeignOwner`] for a resource belonging to
    /// another generation, or [`ResourceError::DuplicateResource`] when the same
    /// resource id is already reserved for a different operation or intent.
    pub fn reserve(
        &mut self,
        operation: &OperationId,
        digest: &str,
        kind: ResourceKind,
        resource: &TerminalRef,
    ) -> Result<(), ResourceError> {
        if resource.daemon_generation != self.owner {
            return Err(ResourceError::ForeignOwner);
        }
        if let Some(existing) = self.resource(resource) {
            let same = &existing.operation == operation
                && existing.digest == digest
                && existing.kind == kind;
            return if same {
                Ok(())
            } else {
                Err(ResourceError::DuplicateResource)
            };
        }
        self.resources.push(ShardResource {
            resource: resource.clone(),
            kind,
            operation: *operation,
            digest: digest.to_owned(),
            process: None,
            state: ResourceState::Reserved,
            revision: 1,
        });
        Ok(())
    }

    /// Record the OS-verified child of a reserved resource.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`],
    /// [`ResourceError::IdentityUnverifiable`] for an identity that could never
    /// be re-observed, or [`ResourceError::WrongState`] when the record already
    /// holds a different child.
    pub fn record_spawn(
        &mut self,
        resource: &TerminalRef,
        identity: &ChildIdentity,
    ) -> Result<(), ResourceError> {
        if !identity.is_verifiable() {
            return Err(ResourceError::IdentityUnverifiable);
        }
        let entry = self.resource_mut(resource)?;
        match entry.state {
            ResourceState::Reserved => {
                entry.process = Some(identity.clone());
                entry.state = ResourceState::Running;
                entry.revision += 1;
                Ok(())
            }
            ResourceState::Running if entry.process.as_ref() == Some(identity) => Ok(()),
            _ => Err(ResourceError::WrongState),
        }
    }

    /// Mark a record whose ownership cannot be proved. It stays visible and
    /// non-live: no spawn, kill, or capacity release is inferred from it.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`].
    pub fn mark_ownership_unknown(&mut self, resource: &TerminalRef) -> Result<(), ResourceError> {
        let entry = self.resource_mut(resource)?;
        if entry.state != ResourceState::OwnershipUnknown {
            entry.state = ResourceState::OwnershipUnknown;
            entry.revision += 1;
        }
        Ok(())
    }

    /// Accept a terminal command for one owned resource. In-flight commands gate
    /// this generation's collection.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`] or
    /// [`ResourceError::WrongState`] when the resource holds no live child.
    pub fn accept_command(
        &mut self,
        resource: &TerminalRef,
        command: &OperationId,
    ) -> Result<(), ResourceError> {
        let entry = self.resource_mut(resource)?;
        if entry.state != ResourceState::Running {
            return Err(ResourceError::WrongState);
        }
        if self
            .in_flight
            .iter()
            .any(|pending| &pending.command == command)
        {
            return Ok(());
        }
        self.in_flight.push(InFlightCommand {
            resource: resource.clone(),
            command: *command,
        });
        Ok(())
    }

    /// Commit a completed command to this shard and publish it once.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`] or
    /// [`ResourceError::UnknownOperation`] for a command this owner never
    /// accepted.
    pub fn commit_command_completion(
        &mut self,
        resource: &TerminalRef,
        command: &OperationId,
    ) -> Result<(), ResourceError> {
        if self.resource(resource).is_none() {
            return Err(ResourceError::UnknownResource);
        }
        let Some(index) = self
            .in_flight
            .iter()
            .position(|pending| &pending.command == command)
        else {
            // Already completed and published: the outbox (or the consumer)
            // holds the single copy, so this converges without a second event.
            return if self.published_completion(resource, command) {
                Ok(())
            } else {
                Err(ResourceError::UnknownOperation)
            };
        };
        self.in_flight.remove(index);
        self.publish(resource, OwnerEvent::CommandCompleted { command: *command });
        Ok(())
    }

    /// Commit journalled output progress and publish it once per offset.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`].
    pub fn commit_output(
        &mut self,
        resource: &TerminalRef,
        offset: u64,
    ) -> Result<(), ResourceError> {
        if self.resource(resource).is_none() {
            return Err(ResourceError::UnknownResource);
        }
        if self.outbox.iter().any(|event| {
            event.resource.terminal_id == resource.terminal_id
                && event.event == OwnerEvent::Output { offset }
        }) {
            return Ok(());
        }
        self.publish(resource, OwnerEvent::Output { offset });
        Ok(())
    }

    /// Commit a child's exit to this shard and publish it exactly once.
    ///
    /// The write is one transition: the resource becomes `Exited` and the outbox
    /// gains its terminal event together, so no crash can leave an exit that is
    /// recorded but never published, or published but not recorded.
    ///
    /// # Errors
    /// Returns [`ResourceError::UnknownResource`], or
    /// [`ResourceError::WrongState`] when the record never held a child.
    pub fn commit_exit(
        &mut self,
        resource: &TerminalRef,
        status: i32,
    ) -> Result<(), ResourceError> {
        let entry = self.resource_mut(resource)?;
        match entry.state {
            ResourceState::Exited { status: recorded } if recorded == status => return Ok(()),
            ResourceState::Running | ResourceState::Reserved => {}
            _ => return Err(ResourceError::WrongState),
        }
        entry.state = ResourceState::Exited { status };
        entry.revision += 1;
        self.in_flight
            .retain(|pending| pending.resource.terminal_id != resource.terminal_id);
        self.publish(resource, OwnerEvent::Exit { status });
        Ok(())
    }

    /// Drop the outbox entries the active consumer has already applied, and
    /// forget resources whose exit has been fully consumed.
    ///
    /// This is the owner's own write: the consumer never acknowledges into this
    /// document. Returns how many outbox entries were reclaimed.
    pub fn reclaim(&mut self, allocator: &AllocatorDocument) -> usize {
        let before = self.outbox.len();
        self.outbox.retain(|event| {
            allocator
                .consumed_revision(&event.resource)
                .is_none_or(|applied| applied < event.event_revision)
        });
        self.resources.retain(|entry| {
            !matches!(entry.state, ResourceState::Exited { .. })
                || self
                    .outbox
                    .iter()
                    .any(|event| event.resource.terminal_id == entry.resource.terminal_id)
        });
        before - self.outbox.len()
    }

    /// The events the active consumer has not applied yet.
    #[must_use]
    pub fn unacked_outbox(&self) -> usize {
        self.outbox.len()
    }

    fn publish(&mut self, resource: &TerminalRef, event: OwnerEvent) {
        self.event_sequence += 1;
        self.outbox.push(OutboxEvent {
            event_revision: self.event_sequence,
            resource: resource.clone(),
            event,
        });
    }

    fn published_completion(&self, resource: &TerminalRef, command: &OperationId) -> bool {
        self.outbox.iter().any(|event| {
            event.resource.terminal_id == resource.terminal_id
                && event.event == OwnerEvent::CommandCompleted { command: *command }
        })
    }

    fn resource_mut(
        &mut self,
        resource: &TerminalRef,
    ) -> Result<&mut ShardResource, ResourceError> {
        self.resources
            .iter_mut()
            .find(|entry| entry.resource.terminal_id == resource.terminal_id)
            .ok_or(ResourceError::UnknownResource)
    }
}

/// Why a generation is not collectable yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionBlocker {
    /// The shard still owns a reserved or running resource.
    LiveResource,
    /// A terminal command this owner accepted has not completed.
    InFlightCommand,
    /// The active consumer has not applied every published event.
    UnackedOutbox,
    /// The global allocator still holds capacity for this owner.
    CapacityClaim,
}

/// Whether this generation's runtime state is fully drained. Every condition is
/// checked: a zero in one of them is never taken for a zero in the others.
///
/// # Errors
/// Returns the first [`CollectionBlocker`] that still holds.
pub fn collectable(
    shard: &ShardDocument,
    allocator: &AllocatorDocument,
) -> Result<(), CollectionBlocker> {
    if shard.live_resources() > 0 {
        return Err(CollectionBlocker::LiveResource);
    }
    if !shard.in_flight.is_empty() {
        return Err(CollectionBlocker::InFlightCommand);
    }
    if shard.unacked_outbox() > 0 {
        return Err(CollectionBlocker::UnackedOutbox);
    }
    if allocator.owner_claims(shard.owner) > 0 {
        return Err(CollectionBlocker::CapacityClaim);
    }
    Ok(())
}

/// What a standby read, and the exact revisions it read it at.
///
/// Holding one of these proves only that a read happened. It carries no write
/// capability, which is the point: a standby's readiness must not reconcile,
/// save, tick, or spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedHydrate {
    owner: DaemonGeneration,
    shard_revision: u64,
    allocator_revision: u64,
    resources: usize,
}

impl SealedHydrate {
    /// The generation this seal describes.
    #[must_use]
    pub fn owner(&self) -> DaemonGeneration {
        self.owner
    }

    /// The shard revision the standby read.
    #[must_use]
    pub fn shard_revision(&self) -> u64 {
        self.shard_revision
    }

    /// The allocator revision the standby read.
    #[must_use]
    pub fn allocator_revision(&self) -> u64 {
        self.allocator_revision
    }

    /// How many records were hydrated.
    #[must_use]
    pub fn resources(&self) -> usize {
        self.resources
    }
}

/// The right to write one shard, granted only after activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterLease {
    owner: DaemonGeneration,
    shard_revision: u64,
    allocator_revision: u64,
}

impl WriterLease {
    /// The generation allowed to write.
    #[must_use]
    pub fn owner(&self) -> DaemonGeneration {
        self.owner
    }

    /// The shard revision admission starts from.
    #[must_use]
    pub fn shard_revision(&self) -> u64 {
        self.shard_revision
    }

    /// The allocator revision admission starts from.
    #[must_use]
    pub fn allocator_revision(&self) -> u64 {
        self.allocator_revision
    }
}

/// Read one generation's shard without writing anything.
///
/// # Errors
/// Returns [`ResourceError::ForeignOwner`] when the stored document names a
/// different generation, or any load failure.
pub fn hydrate(
    shard: &OwnerShard,
    allocator: &AllocatorDocument,
) -> Result<SealedHydrate, ResourceFailure> {
    let snapshot = shard.load()?;
    let document = snapshot.document();
    if document.owner != shard.owner() {
        return Err(ResourceError::ForeignOwner.into());
    }
    Ok(SealedHydrate {
        owner: document.owner,
        shard_revision: document.revision,
        allocator_revision: allocator.revision,
        resources: document.resources.len(),
    })
}

/// Turn a seal into a writer lease after the handoff has committed.
///
/// Both revisions are re-verified: if either object moved between hydrate and
/// activation, the standby's picture is stale and admission does not start.
///
/// # Errors
/// Returns [`ResourceError::SealedElsewhere`] when a revision moved, or any load
/// failure.
pub fn open_writer(
    shard: &OwnerShard,
    allocator: &AllocatorDocument,
    sealed: &SealedHydrate,
) -> Result<WriterLease, ResourceFailure> {
    let snapshot = shard.load()?;
    let document = snapshot.document();
    if document.owner != sealed.owner
        || document.revision != sealed.shard_revision
        || allocator.revision != sealed.allocator_revision
    {
        return Err(ResourceError::SealedElsewhere.into());
    }
    Ok(WriterLease {
        owner: sealed.owner,
        shard_revision: sealed.shard_revision,
        allocator_revision: sealed.allocator_revision,
    })
}

/// One generation's shard over a [`CasFile`].
pub struct OwnerShard {
    store: CasStore<ShardDocument>,
    owner: DaemonGeneration,
}

impl OwnerShard {
    /// Bind the shard of `owner`.
    pub fn new(file: impl CasFile + Send + 'static, owner: DaemonGeneration) -> Self {
        Self {
            store: CasStore::new(file),
            owner,
        }
    }

    /// The generation that owns this shard.
    #[must_use]
    pub fn owner(&self) -> DaemonGeneration {
        self.owner
    }

    /// Read and validate the shard; an absent document is an empty shard.
    ///
    /// # Errors
    /// Returns the store's read error or the document's validation refusal.
    pub fn load(&self) -> Result<CasSnapshot<ShardDocument>, ResourceFailure> {
        let owner = self.owner;
        self.store.load(move || ShardDocument::empty(owner))
    }

    /// Apply `change` under one compare-and-swap.
    ///
    /// # Errors
    /// Returns [`ResourceError::ForeignOwner`] when the stored document belongs
    /// to another generation, `change`'s refusal, or the store's failure.
    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut ShardDocument) -> Result<T, ResourceError>,
    ) -> Result<(T, CasSnapshot<ShardDocument>), ResourceFailure> {
        let owner = self.owner;
        self.store.update(
            move || ShardDocument::empty(owner),
            move |document| {
                owned_by(document, owner)?;
                change(document)
            },
        )
    }
}

/// Refuse a document that belongs to another generation. It is deliberately not
/// part of the generic `update` body: one branch, one compiled copy.
fn owned_by(document: &ShardDocument, owner: DaemonGeneration) -> Result<(), ResourceError> {
    if document.owner == owner {
        Ok(())
    } else {
        Err(ResourceError::ForeignOwner)
    }
}

#[cfg(test)]
mod tests;

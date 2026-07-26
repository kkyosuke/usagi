//! The launch protocol: one write order, one named boundary per crash point.
//!
//! A launch touches three independent things — the shared allocator, the owner's
//! shard, and an OS process — so no single write can move them together. This
//! module fixes the order and states what each boundary means after a `SIGKILL`.
//! Recovery then never guesses: it reads both documents, observes the child, and
//! resumes the one safe continuation.
//!
//! ```text
//! L1  allocator CAS   claim reserved + operation reserved     authority leads the effect
//! L2  shard CAS       owner reservation reserved              owner may now spawn
//! L3  spawn           the child, then observe its identity    the only irreversible step
//! L4  shard CAS       resource running + verified identity    the child is owned durably
//! L5  allocator CAS   operation final (spawned)               the producer's answer is durable
//! ```
//!
//! | crash boundary | durable state | recovery |
//! |---|---|---|
//! | before L1 | nothing | retry is a fresh admission |
//! | L1..L2 | claim only | complete the reservation, then spawn once |
//! | L2..L3 | claim + reservation | spawn once; nothing was spawned yet |
//! | L3..L4 | claim + reservation, child alive | the child is unrecorded: report ambiguous, never spawn a replacement |
//! | L4..L5 | running record | commit the final from the record |
//! | after L5 | final | replay the final verbatim |
//!
//! Two rules make this safe rather than merely ordered. First, the claim is
//! always written *before* the reservation, so a reservation without a claim can
//! only mean state was lost or forged: that is
//! [`LeakReason::ReservationWithoutClaim`] and it fails closed instead of
//! releasing or re-spawning. Second, a child is only ever adopted on an exact
//! identity observation, so a reused PID never becomes somebody else's owner.

use usagi_core::domain::id::{OperationId, TerminalRef};

use crate::usecase::resources::allocator::{
    AllocatorDocument, LaunchFailure, OperationOutcome, ResourceAllocator, ResourceKind,
};
use crate::usecase::resources::identity::{
    ChildIdentity, ChildObservation, ChildProcessProbe, observe_child,
};
use crate::usecase::resources::retention::{LogicalClock, RetentionLimits, admission_guard};
use crate::usecase::resources::shard::{OwnerShard, ResourceState, ShardDocument};
use crate::usecase::resources::{ResourceError, ResourceFailure};

/// One canonical launch intent, keyed by the producer's durable operation id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchIntent {
    /// The producer-issued id, carried end to end from the UI effect.
    pub operation: OperationId,
    /// Digest of the canonical intent (scope, profile, geometry). A different
    /// digest under the same id is an idempotency conflict.
    pub digest: String,
    pub kind: ResourceKind,
    /// The resource identity to use when this operation is admitted fresh. A
    /// replay answers with the recorded identity and ignores this one.
    pub resource: TerminalRef,
}

/// Why exactly one side of a two-object write survived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeakReason {
    /// The owner holds a reservation the allocator never claimed.
    ReservationWithoutClaim,
    /// The claim and the shard disagree about who owns the resource.
    OwnerMismatch,
    /// The shard reservation records a different operation or intent.
    IntentMismatch,
}

/// The one safe continuation for a launch, decided from durable state plus an
/// exact child observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchStep {
    /// The operation already has a durable answer: return exactly this.
    Replay {
        resource: TerminalRef,
        outcome: OperationOutcome,
        revision: u64,
    },
    /// Take (or complete) the claim and the reservation, then spawn once.
    Reserve,
    /// Both sides are durable and no child exists yet.
    Spawn { resource: TerminalRef },
    /// A child was proved spawned; only the final is missing.
    CommitFinal { resource: TerminalRef },
    /// A child may exist and the platform cannot say. Capacity stays held.
    CommitAmbiguous { resource: TerminalRef },
    /// Fail closed: no spawn, no release, no guess.
    Leaked(LeakReason),
}

/// What an accepted launch tells the producer. The same producer id and the same
/// durable revision come back for the original call and for every replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAccepted {
    pub operation: OperationId,
    pub resource: TerminalRef,
    pub outcome: OperationOutcome,
    /// The operation record's durable revision.
    pub revision: u64,
    /// Whether *this* call spawned the child. It is true at most once per
    /// operation, which is what keeps the spawn count at one.
    pub spawned: bool,
}

/// Why a spawn attempt did not produce an owned child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnRefusal {
    /// No process exists. Capacity may be released.
    Definite,
    /// A process may exist but its identity could not be observed. Capacity is
    /// kept and the outcome is ambiguous.
    Ambiguous,
}

/// Spawns the child of one reserved resource.
///
/// The implementation must return an identity it read from the OS after the
/// spawn; when it cannot, it reports [`SpawnRefusal::Ambiguous`] rather than a
/// fabricated token.
pub trait ResourceSpawner {
    /// Spawn the child for `resource`.
    ///
    /// # Errors
    /// Returns the [`SpawnRefusal`] describing what is known about the process.
    fn spawn(&mut self, resource: &TerminalRef) -> Result<ChildIdentity, SpawnRefusal>;
}

/// Decide the one safe continuation for `intent`.
///
/// # Errors
/// Returns [`ResourceError::OperationExpired`] for an id that can never be
/// admitted again, or [`ResourceError::OperationConflict`] when the same id
/// carries a different canonical intent.
pub fn plan_launch(
    allocator: &AllocatorDocument,
    shard: &ShardDocument,
    intent: &LaunchIntent,
    observe: &mut dyn FnMut(&ChildIdentity) -> ChildObservation,
) -> Result<LaunchStep, ResourceError> {
    if allocator.is_expired(&intent.operation) {
        return Err(ResourceError::OperationExpired);
    }
    let Some(record) = allocator.operation(&intent.operation) else {
        // No claim exists. A shard reservation for the same operation therefore
        // has no authority behind it, which is never resolved by guessing.
        return if shard
            .resources
            .iter()
            .any(|entry| entry.operation == intent.operation)
        {
            Ok(LaunchStep::Leaked(LeakReason::ReservationWithoutClaim))
        } else {
            Ok(LaunchStep::Reserve)
        };
    };
    if record.digest != intent.digest {
        return Err(ResourceError::OperationConflict);
    }
    if record.outcome.is_final() {
        return Ok(LaunchStep::Replay {
            resource: record.resource.clone(),
            outcome: record.outcome,
            revision: record.revision,
        });
    }
    if record.owner != shard.owner {
        return Ok(LaunchStep::Leaked(LeakReason::OwnerMismatch));
    }
    let resource = record.resource.clone();
    let Some(entry) = shard.resource(&resource) else {
        return Ok(LaunchStep::Reserve);
    };
    if entry.operation != intent.operation || entry.digest != intent.digest {
        return Ok(LaunchStep::Leaked(LeakReason::IntentMismatch));
    }
    Ok(match entry.state {
        ResourceState::Reserved => LaunchStep::Spawn { resource },
        // A child that is proved gone still proves it *was* spawned, so the
        // final is committed here and the exit path releases the capacity.
        ResourceState::Running => match entry.process.as_ref().map(&mut *observe) {
            Some(ChildObservation::Exact) => LaunchStep::CommitFinal { resource },
            Some(observation) if observation.is_definitely_gone() => {
                LaunchStep::CommitFinal { resource }
            }
            _ => LaunchStep::CommitAmbiguous { resource },
        },
        ResourceState::Exited { .. } => LaunchStep::CommitFinal { resource },
        ResourceState::OwnershipUnknown => LaunchStep::CommitAmbiguous { resource },
    })
}

/// Run the launch protocol to its next durable stop.
///
/// Calling this repeatedly for the same operation converges: the spawn happens at
/// most once, and every later call replays the durable final with the same
/// producer id and revision.
///
/// # Errors
/// Returns [`ResourceError::OperationExpired`],
/// [`ResourceError::OperationConflict`], [`ResourceError::CapacityExhausted`],
/// [`ResourceError::RetentionBackpressure`], [`ResourceError::OwnershipUnknown`]
/// for a one-sided state, or a store failure.
pub fn execute_launch(
    allocator: &ResourceAllocator,
    shard: &OwnerShard,
    intent: &LaunchIntent,
    spawner: &mut dyn ResourceSpawner,
    probe: &dyn ChildProcessProbe,
    clock: &dyn LogicalClock,
    limits: &RetentionLimits,
) -> Result<LaunchAccepted, ResourceFailure> {
    let allocator_document = allocator.load()?.to_document();
    let shard_document = shard.load()?.to_document();
    let mut observe = |identity: &ChildIdentity| observe_child(probe, identity);
    let step = plan_launch(&allocator_document, &shard_document, intent, &mut observe)?;
    match step {
        LaunchStep::Replay {
            resource,
            outcome,
            revision,
        } => Ok(LaunchAccepted {
            operation: intent.operation,
            resource,
            outcome,
            revision,
            spawned: false,
        }),
        LaunchStep::Leaked(_) => Err(ResourceError::OwnershipUnknown.into()),
        LaunchStep::Reserve => {
            admission_guard(&allocator_document, limits, clock.now())?;
            let resource = reserve(allocator, shard, intent)?;
            spawn_once(allocator, shard, intent, &resource, spawner, clock)
        }
        LaunchStep::Spawn { resource } => {
            spawn_once(allocator, shard, intent, &resource, spawner, clock)
        }
        LaunchStep::CommitFinal { resource } => {
            commit_spawned(allocator, intent, &resource, clock, false)
        }
        LaunchStep::CommitAmbiguous { resource } => {
            commit_ambiguous(allocator, intent, &resource, clock)
        }
    }
}

/// L1 then L2: the claim always becomes durable before the reservation does.
///
/// Returns the *claimed* resource identity. A resumed launch keeps the identity
/// its claim already names, so a retry with a freshly minted proposal cannot
/// produce a second resource for one operation.
fn reserve(
    allocator: &ResourceAllocator,
    shard: &OwnerShard,
    intent: &LaunchIntent,
) -> Result<TerminalRef, ResourceFailure> {
    let policy = allocator.policy();
    let (resource, _) = allocator.update(|document| {
        document.reserve(
            &intent.operation,
            &intent.digest,
            intent.kind,
            shard.owner(),
            &intent.resource,
            policy,
        )?;
        document
            .operation(&intent.operation)
            .map(|record| record.resource.clone())
            .ok_or(ResourceError::UnknownOperation)
    })?;
    shard.update(|document| {
        document.reserve(&intent.operation, &intent.digest, intent.kind, &resource)
    })?;
    Ok(resource)
}

/// L3 then L4/L5. The spawn is the only irreversible step, and it runs exactly
/// once because both durable sides already record this operation.
fn spawn_once(
    allocator: &ResourceAllocator,
    shard: &OwnerShard,
    intent: &LaunchIntent,
    resource: &TerminalRef,
    spawner: &mut dyn ResourceSpawner,
    clock: &dyn LogicalClock,
) -> Result<LaunchAccepted, ResourceFailure> {
    match spawner.spawn(resource) {
        Ok(identity) => {
            if shard
                .update(|document| document.record_spawn(resource, &identity))
                .is_err()
            {
                // The child exists but its record does not — the persist-after-spawn
                // case. It is answered as a durable ambiguous final, never as a
                // failure that a retry could turn into a second child, and the
                // capacity stays claimed because a process may be running.
                return commit_ambiguous(allocator, intent, resource, clock);
            }
            commit_spawned(allocator, intent, resource, clock, true)
        }
        Err(SpawnRefusal::Definite) => {
            let now = clock.now();
            let (revision, _) = allocator.update(|document| {
                document.mark_failed(&intent.operation, LaunchFailure::Spawn, now)?;
                Ok(record_revision(document, &intent.operation))
            })?;
            Ok(LaunchAccepted {
                operation: intent.operation,
                resource: resource.clone(),
                outcome: OperationOutcome::Failed(LaunchFailure::Spawn),
                revision,
                spawned: false,
            })
        }
        Err(SpawnRefusal::Ambiguous) => {
            // A process may exist. The record is marked unknown so nothing
            // signals or releases it, and the producer gets a durable final.
            shard.update(|document| document.mark_ownership_unknown(resource))?;
            commit_ambiguous(allocator, intent, resource, clock)
        }
    }
}

/// L5: the producer's answer becomes durable.
fn commit_spawned(
    allocator: &ResourceAllocator,
    intent: &LaunchIntent,
    resource: &TerminalRef,
    clock: &dyn LogicalClock,
    spawned: bool,
) -> Result<LaunchAccepted, ResourceFailure> {
    let now = clock.now();
    let (revision, _) = allocator.update(|document| {
        document.mark_spawned(&intent.operation, now)?;
        Ok(record_revision(document, &intent.operation))
    })?;
    Ok(LaunchAccepted {
        operation: intent.operation,
        resource: resource.clone(),
        outcome: OperationOutcome::Spawned,
        revision,
        spawned,
    })
}

fn commit_ambiguous(
    allocator: &ResourceAllocator,
    intent: &LaunchIntent,
    resource: &TerminalRef,
    clock: &dyn LogicalClock,
) -> Result<LaunchAccepted, ResourceFailure> {
    let now = clock.now();
    let (revision, _) = allocator.update(|document| {
        document.mark_ambiguous(&intent.operation, now)?;
        Ok(record_revision(document, &intent.operation))
    })?;
    Ok(LaunchAccepted {
        operation: intent.operation,
        resource: resource.clone(),
        outcome: OperationOutcome::Ambiguous,
        revision,
        spawned: false,
    })
}

fn record_revision(document: &AllocatorDocument, operation: &OperationId) -> u64 {
    document
        .operation(operation)
        .map_or(0, |record| record.revision)
}

#[cfg(test)]
mod tests;

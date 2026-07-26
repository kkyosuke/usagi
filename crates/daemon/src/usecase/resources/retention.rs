//! Bounded retention, expiry, and collection of the operation ledger.
//!
//! Idempotency needs memory, and memory grows with every launch. This module is
//! what keeps the allocator's ledger bounded *without ever replaying a wrong
//! answer*, in three ordered phases with a durable stop between each:
//!
//! ```text
//! G1  allocator CAS   full outcome ──▶ compact tombstone      exact answer replaced atomically
//! G2  allocator CAS   expiry watermark advances               ids at/below it are expired forever
//! G3  allocator CAS   tombstones at/below the watermark drop  the last bytes are released
//! ```
//!
//! | phase | what a retry gets afterwards |
//! |---|---|
//! | before G1, inside the window | the full exact final, replayed verbatim |
//! | after G1 | typed `operation_expired`, effect zero |
//! | after G2 | typed `operation_expired`, from the watermark alone |
//! | after G3 | typed `operation_expired`, still from the watermark |
//!
//! The watermark is what makes G3 safe: an id may only lose its tombstone once
//! the watermark guarantees it can never be admitted as fresh again. The
//! watermark therefore advances only from ids the server itself already sealed,
//! never from a timestamp a client sent, and never past an operation that is
//! still live — otherwise a running launch would be declared expired.
//!
//! Nothing is collected on age alone: a record must be a *collectable* final
//! (never ambiguous), its capacity must already be released exactly once, and its
//! consumer dependencies must be zero. When the hard caps are reached and no
//! record satisfies that, the ledger does not evict anything — fresh launches are
//! refused with [`ResourceError::RetentionBackpressure`] instead
//! ([`admission_guard`]).

use usagi_core::domain::id::OperationId;

use crate::usecase::resources::ResourceError;
use crate::usecase::resources::allocator::{
    AllocatorDocument, ClaimState, ExpiryClass, OperationOutcome, OperationTombstone,
    ResourceAllocator, precedes_or_equals,
};
use crate::usecase::resources::{CasFile, ResourceFailure};

/// Monotonic logical time. Production binds it to a coarse counter or clock; the
/// retention tests inject a fake so every phase boundary is deterministic.
pub trait LogicalClock {
    /// The current logical time.
    fn now(&self) -> u64;
}

/// The hard limits and the guaranteed windows of the operation ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionLimits {
    /// Hard cap on retained records (full outcomes plus tombstones).
    pub max_operations: usize,
    /// Hard cap on the serialized document size.
    pub max_bytes: usize,
    /// A collectable final older than this is collected even below the caps.
    pub max_age: u64,
    /// The documented minimum idempotency window: inside it, the same operation
    /// replays its full exact outcome.
    pub min_window: u64,
    /// How long an exact tombstone is kept after its full outcome was evicted.
    pub expiry_horizon: u64,
}

impl RetentionLimits {
    /// Build limits, keeping `min_window` as the floor of the expiry horizon so a
    /// configuration cannot promise less replay safety than the window states.
    #[must_use]
    pub fn new(
        max_operations: usize,
        max_bytes: usize,
        max_age: u64,
        min_window: u64,
        expiry_horizon: u64,
    ) -> Self {
        Self {
            max_operations,
            max_bytes,
            max_age,
            min_window,
            expiry_horizon: expiry_horizon.max(min_window),
        }
    }
}

/// One collection phase. Each is applied as its own compare-and-swap, so a crash
/// between phases leaves a state the next pass rolls forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcPhase {
    /// Replace these full outcomes with compact tombstones.
    Evict(Vec<OperationId>),
    /// Advance the durable expiry watermark to this id.
    AdvanceWatermark(OperationId),
    /// Drop these exact tombstones, which the watermark already covers.
    Compact(Vec<OperationId>),
}

/// What a collection pass may safely do right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GcPlan {
    pub phases: Vec<GcPhase>,
    /// The caps are reached and no phase is safe, so fresh admission must be
    /// refused rather than a retained record evicted.
    pub backpressure: bool,
}

/// What a collection pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcReport {
    pub evicted: usize,
    pub compacted: usize,
    pub watermark_advanced: bool,
    pub backpressure: bool,
}

/// The serialized size of the ledger, which is what the byte cap bounds.
#[must_use]
pub fn serialized_bytes(document: &AllocatorDocument) -> usize {
    serde_json::to_vec(document).unwrap_or_default().len()
}

/// Refuse a fresh admission that would grow a ledger which is already at a hard
/// cap and cannot be collected safely.
///
/// # Errors
/// Returns [`ResourceError::RetentionBackpressure`], which is effect zero: no
/// reservation, no capacity claim, and no spawn happen.
pub fn admission_guard(
    document: &AllocatorDocument,
    limits: &RetentionLimits,
    now: u64,
) -> Result<(), ResourceError> {
    if plan_gc(document, limits, now).backpressure {
        return Err(ResourceError::RetentionBackpressure);
    }
    Ok(())
}

/// Plan the phases this ledger may run at `now`.
#[must_use]
pub fn plan_gc(document: &AllocatorDocument, limits: &RetentionLimits, now: u64) -> GcPlan {
    let retained = document.operations.len() + document.tombstones.len();
    let over_cap =
        retained >= limits.max_operations || serialized_bytes(document) >= limits.max_bytes;
    let evictable: Vec<OperationId> = document
        .operations
        .iter()
        .filter(|record| is_evictable(document, &record.operation, limits, now))
        .filter(|record| {
            over_cap
                || record
                    .sealed_at
                    .is_some_and(|sealed| sealed.saturating_add(limits.max_age) <= now)
        })
        .map(|record| record.operation)
        .collect();
    let mut phases = Vec::new();
    if !evictable.is_empty() {
        phases.push(GcPhase::Evict(evictable.clone()));
    }
    // The watermark may only cover ids that no still-live operation sits at or
    // below — including the ones this pass is about to evict.
    let live_floor = document
        .operations
        .iter()
        .filter(|record| !evictable.contains(&record.operation))
        .map(|record| record.operation.as_str())
        .min();
    let candidate: Option<&OperationId> = document
        .tombstones
        .iter()
        .map(|tombstone| &tombstone.operation)
        .chain(evictable.iter())
        .filter(|operation| {
            live_floor
                .as_deref()
                .is_none_or(|floor| operation.as_str().as_str() < floor)
        })
        .max_by_key(|operation| operation.as_str());
    if let Some(candidate) = candidate
        && document
            .watermark
            .as_ref()
            .is_none_or(|current| current != candidate && precedes_or_equals(current, candidate))
    {
        phases.push(GcPhase::AdvanceWatermark(*candidate));
    }
    // Compaction is measured against the watermark that will be in effect after
    // this pass, so a plan is applied in the order it was planned.
    let effective = candidate.or(document.watermark.as_ref());
    let compactable: Vec<OperationId> = document
        .tombstones
        .iter()
        .filter(|tombstone| {
            tombstone.cutoff.saturating_add(limits.expiry_horizon) <= now
                && effective
                    .is_some_and(|watermark| precedes_or_equals(&tombstone.operation, watermark))
        })
        .map(|tombstone| tombstone.operation)
        .collect();
    if !compactable.is_empty() {
        phases.push(GcPhase::Compact(compactable));
    }
    GcPlan {
        backpressure: over_cap && phases.is_empty(),
        phases,
    }
}

/// Apply one planned phase to the document.
///
/// # Errors
/// Returns [`ResourceError::WrongState`] when a candidate stopped being safe
/// between planning and applying — a concurrent retry or ACK always wins over a
/// collection.
pub fn apply_phase(
    document: &mut AllocatorDocument,
    phase: &GcPhase,
    limits: &RetentionLimits,
    now: u64,
) -> Result<(), ResourceError> {
    match phase {
        GcPhase::Evict(operations) => {
            for operation in operations {
                evict(document, operation, limits, now)?;
            }
            Ok(())
        }
        GcPhase::AdvanceWatermark(candidate) => {
            if document
                .watermark
                .as_ref()
                .is_some_and(|current| precedes_or_equals(candidate, current))
            {
                return Ok(());
            }
            document.watermark = Some(*candidate);
            Ok(())
        }
        GcPhase::Compact(operations) => {
            let watermark = document.watermark.ok_or(ResourceError::WrongState)?;
            for operation in operations {
                if !precedes_or_equals(operation, &watermark) {
                    return Err(ResourceError::WrongState);
                }
            }
            document
                .tombstones
                .retain(|tombstone| !operations.contains(&tombstone.operation));
            Ok(())
        }
    }
}

/// Run every safe phase, one compare-and-swap per phase.
///
/// # Errors
/// Returns a store failure. A phase that stopped being safe is dropped rather
/// than forced, so a retry racing collection keeps its full outcome.
pub fn collect_garbage<F: CasFile>(
    allocator: &ResourceAllocator<F>,
    limits: &RetentionLimits,
    clock: &dyn LogicalClock,
) -> Result<GcReport, ResourceFailure> {
    let now = clock.now();
    let plan = plan_gc(&allocator.load()?.to_document(), limits, now);
    let mut report = GcReport {
        backpressure: plan.backpressure,
        ..GcReport::default()
    };
    for phase in &plan.phases {
        // `apply_phase` refuses only with `WrongState`, which means a concurrent
        // retry or ACK won the race: the phase is dropped, never forced.
        let (applied, _) =
            allocator.update(|document| Ok(apply_phase(document, phase, limits, now).is_ok()))?;
        if !applied {
            // The phases of one plan depend on each other — a watermark only
            // covers what the eviction before it removed — so a dropped phase
            // ends the pass instead of letting a later one run on a state its
            // predecessor never produced. The next pass re-plans from scratch.
            break;
        }
        match phase {
            GcPhase::Evict(operations) => report.evicted += operations.len(),
            GcPhase::AdvanceWatermark(_) => report.watermark_advanced = true,
            GcPhase::Compact(operations) => report.compacted += operations.len(),
        }
    }
    Ok(report)
}

/// Whether one full outcome may be replaced by a tombstone right now.
fn is_evictable(
    document: &AllocatorDocument,
    operation: &OperationId,
    limits: &RetentionLimits,
    now: u64,
) -> bool {
    let Some(record) = document
        .operations
        .iter()
        .find(|record| &record.operation == operation)
    else {
        return false;
    };
    // Ambiguous finals and anything not yet final stay forever: a child may
    // exist, so its exact answer is the only safe one.
    if !record.outcome.is_collectable() {
        return false;
    }
    // A final always carries its seal, so the window is checked on the option
    // itself rather than through a branch that cannot be reached. Capacity must
    // also already have been released exactly once, which is what proves the
    // consumer applied the owner's terminal event.
    record
        .sealed_at
        .is_some_and(|sealed| sealed.saturating_add(limits.min_window) <= now)
        && document
            .claim(&record.resource)
            .is_none_or(|claim| claim.state == ClaimState::Released)
}

fn evict(
    document: &mut AllocatorDocument,
    operation: &OperationId,
    limits: &RetentionLimits,
    now: u64,
) -> Result<(), ResourceError> {
    if document.tombstone(operation).is_some() {
        return Ok(());
    }
    if !is_evictable(document, operation, limits, now) {
        return Err(ResourceError::WrongState);
    }
    let record = document
        .operation(operation)
        .ok_or(ResourceError::UnknownOperation)?
        .clone();
    let class = match record.outcome {
        OperationOutcome::Spawned => ExpiryClass::Spawned,
        _ => ExpiryClass::Failed,
    };
    document.tombstones.push(OperationTombstone {
        operation: *operation,
        digest: record.digest,
        class,
        cutoff: now,
    });
    document
        .operations
        .retain(|retained| &retained.operation != operation);
    let resource = record.resource.terminal_id.as_str();
    document
        .claims
        .retain(|claim| claim.resource.terminal_id.as_str() != resource);
    document
        .consumed
        .retain(|event| event.resource.terminal_id.as_str() != resource);
    Ok(())
}

#[cfg(test)]
mod tests;

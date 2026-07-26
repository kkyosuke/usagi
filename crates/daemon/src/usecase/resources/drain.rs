//! Handing a draining owner's events to the active consumer.
//!
//! The draining generation still owns its children, so it is the only writer of
//! its own shard. The active generation owns capacity and the projection clients
//! read, so it is the only writer of the allocator. Neither writes the other's
//! document — that is the whole reason a lost update cannot happen here:
//!
//! ```text
//! E1  old shard CAS    resource exited + outbox event(rev)   owner publishes once
//! E2  allocator CAS    consumed(rev) + claim released        active applies once
//! E3  old shard CAS    outbox entries <= consumed dropped    owner reclaims its own outbox
//! ```
//!
//! | crash boundary | durable state | recovery |
//! |---|---|---|
//! | before E1 | nothing | the child's exit is re-observed and published |
//! | E1..E2 | event published | the consumer applies it; redelivery is idempotent |
//! | E2..E3 | event applied | the owner reclaims on its next pass |
//! | after E3 | nothing pending | the resource is forgotten |
//!
//! [`ActiveConsumer`] borrows the old shard *immutably*. That is the structural
//! proof of "the active generation never writes the old shard": there is no
//! method on it that could, and an acknowledgement is expressed as the allocator's
//! consumed revision, which the owner reads back for itself.

use usagi_core::domain::id::DaemonGeneration;

use crate::usecase::resources::allocator::{ConsumeOutcome, ResourceAllocator};
use crate::usecase::resources::shard::{OutboxEvent, OwnerShard, ShardDocument};
use crate::usecase::resources::{ResourceError, ResourceFailure};

/// What consuming one shard's outbox did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub struct ConsumeReport {
    /// Events applied for the first time by this pass.
    pub applied: usize,
    /// Events that were already applied (duplicate, reordered, or late).
    pub duplicates: usize,
    /// Events refused because they did not describe this owner's resource.
    pub refused: usize,
}

/// The active generation's read-only view of another generation's outbox.
///
/// It holds the allocator (which it may write) and nothing that could write the
/// old shard.
pub struct ActiveConsumer<'a> {
    allocator: &'a ResourceAllocator,
}

impl<'a> ActiveConsumer<'a> {
    /// Bind a consumer to the allocator it is allowed to write.
    #[must_use]
    pub fn new(allocator: &'a ResourceAllocator) -> Self {
        Self { allocator }
    }

    /// Apply every event a draining owner published, in owner-published order.
    ///
    /// Duplicates, reordered redelivery, and events for a resource this owner
    /// does not own converge on the same outcome and never touch another
    /// resource. Each terminal event releases its capacity exactly once.
    ///
    /// # Errors
    /// Returns a store failure. A per-event refusal is counted in the report
    /// rather than aborting the pass, so one corrupt entry cannot block the
    /// generation's remaining exits.
    pub fn consume(&self, shard: &ShardDocument) -> Result<ConsumeReport, ResourceFailure> {
        let mut ordered: Vec<&OutboxEvent> = shard.outbox.iter().collect();
        ordered.sort_by_key(|event| event.event_revision);
        let mut report = ConsumeReport::default();
        for event in ordered {
            match self.apply(shard.owner, event)? {
                Ok(ConsumeOutcome::Applied) => report.applied += 1,
                Ok(ConsumeOutcome::AlreadyConsumed) => report.duplicates += 1,
                Err(_) => report.refused += 1,
            }
        }
        Ok(report)
    }

    fn apply(
        &self,
        owner: DaemonGeneration,
        event: &OutboxEvent,
    ) -> Result<Result<ConsumeOutcome, ResourceError>, ResourceFailure> {
        let terminal = event.event.is_terminal();
        let outcome = self.allocator.update(|document| {
            let applied = if terminal {
                document.consume_exit(owner, &event.resource, event.event_revision)
            } else {
                document.consume_progress(owner, &event.resource, event.event_revision)
            };
            // A refusal is data here: it must not roll back the events this pass
            // already applied, so it is returned rather than propagated.
            Ok(applied)
        })?;
        Ok(outcome.0)
    }
}

/// E1: the owner commits a child's exit to its own shard and publishes it once.
///
/// # Errors
/// Returns [`ResourceError::UnknownResource`], [`ResourceError::WrongState`], or
/// a store failure.
pub fn publish_exit(
    shard: &OwnerShard,
    resource: &usagi_core::domain::id::TerminalRef,
    status: i32,
) -> Result<(), ResourceFailure> {
    shard.update(|document| document.commit_exit(resource, status))?;
    Ok(())
}

/// E3: the owner drops the outbox entries the allocator records as consumed.
///
/// # Errors
/// Returns a store failure.
pub fn reclaim_outbox(
    shard: &OwnerShard,
    allocator: &ResourceAllocator,
) -> Result<usize, ResourceFailure> {
    let consumed = allocator.load()?.to_document();
    let (reclaimed, _) = shard.update(|document| Ok(document.reclaim(&consumed)))?;
    Ok(reclaimed)
}

#[cfg(test)]
mod tests;

//! What one owner can see of a pool it no longer has to itself.
//!
//! A single daemon owns the whole concurrency pool, so counting its own live
//! records answers "is the pool full?". The moment a planned rollover keeps a
//! draining owner alive next to a new active one, that answer is wrong in the
//! dangerous direction: the new owner sees zero of the old owner's children and
//! happily doubles the configured limit.
//!
//! The global allocator already knows the truth — every retained generation's
//! claims live in one document — so this seam is only about *reading* it from
//! inside a coordinator that must not depend on the allocator's types:
//!
//! ```text
//! own live records          + foreign claims (allocator)  >= limit  ──▶ refuse
//! ^ the coordinator knows     ^ this seam supplies
//! ```
//!
//! An unreadable allocator answers `None`, and a caller that cannot read the
//! shared pool must behave as if it were full: refusing a launch costs a retry,
//! while guessing zero costs a second child the limit was meant to prevent.
//!
//! This gate is the *typed* refusal. It is not the guarantee — two processes can
//! both read a pool with one slot left. The guarantee is the allocator's
//! compare-and-swap in [`super::mirror`], which admits exactly one of them.

use std::fmt;
use std::sync::Arc;

use usagi_core::domain::id::DaemonGeneration;

use crate::usecase::resources::allocator::{ResourceAllocator, ResourceKind};

/// The slots other retained generations hold in one owner's pool.
pub trait ForeignOccupancy: fmt::Debug + Send {
    /// Slots held by every generation other than this owner, or `None` when the
    /// shared document cannot be read.
    fn occupied(&self) -> Option<usize>;
}

/// The allocator-backed view of one pool.
pub struct SharedPool {
    allocator: Arc<ResourceAllocator>,
    owner: DaemonGeneration,
    kind: ResourceKind,
}

impl SharedPool {
    /// Read `kind`'s pool from `allocator`, excluding `owner`'s own claims.
    #[must_use]
    pub fn new(
        allocator: Arc<ResourceAllocator>,
        owner: DaemonGeneration,
        kind: ResourceKind,
    ) -> Self {
        Self {
            allocator,
            owner,
            kind,
        }
    }
}

impl fmt::Debug for SharedPool {
    /// The allocator is a durable seam rather than a value, so it is named as
    /// one: printing a document on every diagnostic would be noise, and the
    /// coordinator that holds this pool has to stay printable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedPool")
            .field("owner", &self.owner)
            .field("pool", &self.kind.pool())
            .finish_non_exhaustive()
    }
}

impl ForeignOccupancy for SharedPool {
    fn occupied(&self) -> Option<usize> {
        let snapshot = self.allocator.load().ok()?;
        Some(snapshot.document().foreign_pool_used(self.kind, self.owner))
    }
}

/// The occupancy a coordinator must assume for its pool.
///
/// It is a free function rather than a method so both coordinators share one
/// compiled copy of the fail-closed rule, and so the rule can be tested without
/// building a coordinator.
#[must_use]
pub fn foreign_occupancy(source: Option<&dyn ForeignOccupancy>, limit: usize) -> usize {
    // No shared pool at all means this process is the only generation, which is
    // the single-daemon case the limit was originally written for.
    source.map_or(0, |source| source.occupied().unwrap_or(limit))
}

#[cfg(test)]
mod tests;

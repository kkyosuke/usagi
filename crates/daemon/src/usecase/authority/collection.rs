//! Collecting a draining generation once its runtime is fully drained.
//!
//! A planned handoff leaves the old process alive and `draining` so its PTYs
//! survive the replacement. That process keeps its socket, its registry entry,
//! and every capacity claim until the last resource it owns is gone — and
//! nothing else can end it, because only the draining owner itself knows when
//! its last child has exited. This module holds the one decision that does:
//! *may this generation retire itself now?*
//!
//! The answer is yes only when both halves agree:
//!
//! | half | where it is decided |
//! |---|---|
//! | this generation is `draining` — not `active` (which still owns runtime) nor `retired` (which already gave it up) | the [`AdmissionGate`]'s role |
//! | its runtime is drained — no live resource, in-flight command, outbox event, or capacity claim | the owner shard and the allocator (`resources::shard::collectable`) |
//!
//! This module never re-derives the drained observation; the caller passes the
//! shard-and-allocator verdict in. What it adds is the authority half: closing
//! the gate, joining the retained client workers, and recording the retirement,
//! in the fixed order [`collect_retired`] documents. The endpoint and the
//! process are the caller's to reclaim afterwards.
//!
//! Splitting "is it drained?" (the resource layer) from "may it retire?" (this
//! layer) is deliberate: getting the first wrong drops a **live PTY**, getting
//! the second wrong leaves an uncollectable generation, and keeping them apart
//! is what makes a failure attributable to one and not the other.

use std::fmt;

use usagi_core::domain::id::DaemonGeneration;

use super::admission::{AdmissionGate, LeaseClass};
use super::registry::GenerationRegistry;
use super::rollover::{HandoffFailure, collect_retired};
use super::workers::{ClientWorkers, RetireReport};
use crate::usecase::generation::GenerationRole;
use crate::usecase::resources::ResourceFailure;
use crate::usecase::resources::durable::ShardedRuntimeState;
use crate::usecase::resources::shard::CollectionBlocker;

/// The owner-shard and allocator observation that gates retirement.
pub trait DrainObservation {
    /// Report the first claim that still blocks this generation, or `None` only
    /// when every collection condition is zero.
    ///
    /// # Errors
    /// Returns a durable-store failure. Uncertainty never means drained.
    fn blocker(&self) -> Result<Option<CollectionBlocker>, ResourceFailure>;
}

impl DrainObservation for ShardedRuntimeState {
    fn blocker(&self) -> Result<Option<CollectionBlocker>, ResourceFailure> {
        self.self_collectable()
    }
}

/// A store observation or authority transition that stopped collection.
#[derive(Debug)]
pub enum CollectionFailure {
    Runtime(ResourceFailure),
    Authority(HandoffFailure),
}

impl From<ResourceFailure> for CollectionFailure {
    fn from(error: ResourceFailure) -> Self {
        Self::Runtime(error)
    }
}

impl From<HandoffFailure> for CollectionFailure {
    fn from(error: HandoffFailure) -> Self {
        Self::Authority(error)
    }
}

impl fmt::Display for CollectionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "draining runtime observation failed: {error}"),
            Self::Authority(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CollectionFailure {}

/// What a collection attempt decided.
#[derive(Debug)]
pub enum Collection {
    /// This generation is not draining, so there is nothing to collect: an
    /// `active` generation still owns its runtime, and a `retired` one already
    /// gave it up. This is the ordinary answer on every tick before a handoff.
    NotDraining,
    /// The generation is draining but its runtime is not drained yet, so it is
    /// left exactly as it was — its PTYs keep being served.
    Pending(CollectionBlocker),
    /// The generation was retired. The caller now reclaims the endpoint and
    /// exits the process; the report names every client worker that was joined.
    Collected(RetireReport),
}

/// Retire this draining generation when `runtime` reports its state is empty.
///
/// `runtime` supplies the owner shard and allocator's verdict
/// (`resources::durable::ShardedRuntimeState::self_collectable`): every owned
/// resource, in-flight command, unconsumed outbox event, and capacity claim is
/// zero. It is read twice: once as the cheap ordinary wait, then again after
/// owner-terminal lease issuance is closed and every lease already issued has
/// drained. The second read is what prevents an exit or completion already in
/// flight from publishing one last event between "zero" and retirement.
///
/// If that final read finds a blocker, the gate stays closed and a later attempt
/// retries the durable observation. It is never reopened: the optimistic read
/// already proved there was no live resource left to serve, and reopening would
/// let a new owner-terminal effect race the next zero observation.
///
/// The role is checked first and against the live gate, so a generation that is
/// still `active` — one whose handoff has not moved it — is never collected even
/// if it happens to own nothing, and a `retired` one is a no-op rather than a
/// second retirement.
///
/// # Errors
/// Returns the [`CollectionFailure`] that stopped the observation or retirement.
/// A precondition that is simply not met yet is reported as
/// [`Collection::NotDraining`] or [`Collection::Pending`], not as an error.
pub fn collect_if_drained(
    registry: &GenerationRegistry,
    gate: &AdmissionGate,
    workers: &ClientWorkers,
    generation: DaemonGeneration,
    runtime: &dyn DrainObservation,
) -> Result<Collection, CollectionFailure> {
    if gate.role() != GenerationRole::Draining {
        return Ok(Collection::NotDraining);
    }
    if let Some(blocker) = runtime.blocker()? {
        return Ok(Collection::Pending(blocker));
    }
    gate.close(LeaseClass::ActiveControl);
    gate.close(LeaseClass::OwnerTerminal);
    gate.await_drain(LeaseClass::ActiveControl)
        .map_err(HandoffFailure::from)?;
    gate.await_drain(LeaseClass::OwnerTerminal)
        .map_err(HandoffFailure::from)?;
    if let Some(blocker) = runtime.blocker()? {
        return Ok(Collection::Pending(blocker));
    }
    let report = collect_retired(registry, gate, workers, generation)?;
    Ok(Collection::Collected(report))
}

#[cfg(test)]
mod tests;

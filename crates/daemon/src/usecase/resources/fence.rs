//! The inventory of *other* shared writers, and the fence over them.
//!
//! Splitting the runtime records into shards fixes the store that a rollover
//! obviously races. It would be a false fix if the same lost update simply moved
//! to the next whole-snapshot document a draining process still writes — the PR
//! inventory it refreshes from PTY observation, or the supervisor state its tick
//! recomputes. So the writers are enumerated here, with the mode each one is
//! written in, and one rule decides what a generation may do to them.
//!
//! | shared writer | write mode | draining owner |
//! |---|---|---|
//! | `pr-inventory.json` | whole snapshot | publishes an owner-local event; the active writer applies it |
//! | supervisor state | whole snapshot | refused: the active generation's tick recomputes it |
//! | `sessions.json` | whole snapshot | refused: lifecycle admission already closed for it |
//! | `dispatch.json` | append only under a cross-process lock | allowed |
//! | `inbox/*.jsonl` | append only under a cross-process lock | allowed |
//!
//! Append-only writers need no fence: their cross-process lock plus append
//! semantics cannot lose an update. Whole-snapshot writers need exactly one
//! writer, which is the active generation.

use std::collections::BTreeMap;

use usagi_core::domain::id::SessionId;
use usagi_core::domain::pr_inventory::PrInventory;
use usagi_core::usecase::pr_inventory::PrInventoryPort;

use crate::usecase::generation::GenerationRole;

/// How a shared document is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// The whole document is replaced from in-memory state. Two writers lose an
    /// update.
    WholeSnapshot,
    /// Entries are appended under a cross-process lock. Two writers do not lose
    /// an update.
    AppendOnly,
}

/// A shared document written by something other than an owner shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedWriter {
    /// `pr-inventory.json`, refreshed from PTY output observation.
    PrInventory,
    /// The supervisor's durable state, recomputed by its tick.
    SupervisorState,
    /// `sessions.json`, the managed session lifecycle reducer's store.
    SessionLifecycle,
    /// `dispatch.json`, the dispatch/run registry.
    DispatchRegistry,
    /// The per-caller completion inboxes.
    CompletionInbox,
}

impl SharedWriter {
    /// How this document is written.
    #[must_use]
    pub fn mode(self) -> WriteMode {
        match self {
            Self::PrInventory | Self::SupervisorState | Self::SessionLifecycle => {
                WriteMode::WholeSnapshot
            }
            Self::DispatchRegistry | Self::CompletionInbox => WriteMode::AppendOnly,
        }
    }

    /// Whether a draining owner can express its update as an owner-local event
    /// for the active single writer to apply, instead of writing the document.
    #[must_use]
    pub fn is_deferrable(self) -> bool {
        matches!(self, Self::PrInventory)
    }
}

/// Every shared writer this build knows about. A new whole-snapshot document must
/// be added here, which is what keeps the inventory from silently going stale.
pub const SHARED_WRITERS: [SharedWriter; 5] = [
    SharedWriter::PrInventory,
    SharedWriter::SupervisorState,
    SharedWriter::SessionLifecycle,
    SharedWriter::DispatchRegistry,
    SharedWriter::CompletionInbox,
];

/// What a generation may do to a shared document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteVerdict {
    /// This generation is the single writer and may write directly.
    Allowed,
    /// This generation must publish an owner-local event instead; the active
    /// single writer applies it.
    DeferToOutbox,
    /// This generation must not write at all.
    Refused,
}

/// Decide what `role` may do to `writer`.
///
/// A standby is read-only by construction, so its verdict is the same as a
/// retired generation's: nothing.
#[must_use]
pub fn shared_write_verdict(writer: SharedWriter, role: GenerationRole) -> WriteVerdict {
    match role {
        GenerationRole::Active => WriteVerdict::Allowed,
        GenerationRole::Draining => match writer.mode() {
            WriteMode::AppendOnly => WriteVerdict::Allowed,
            WriteMode::WholeSnapshot if writer.is_deferrable() => WriteVerdict::DeferToOutbox,
            WriteMode::WholeSnapshot => WriteVerdict::Refused,
        },
        GenerationRole::Standby | GenerationRole::Retired => WriteVerdict::Refused,
    }
}

/// The whole-snapshot documents that would lose an update without this fence.
#[must_use]
pub fn fenced_writers() -> Vec<SharedWriter> {
    SHARED_WRITERS
        .into_iter()
        .filter(|writer| writer.mode() == WriteMode::WholeSnapshot)
        .collect()
}

/// Why a fenced write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceRefusal {
    pub writer: SharedWriter,
    pub role: GenerationRole,
}

impl std::fmt::Display for FenceRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?} may not be written by a {:?} generation",
            self.writer, self.role
        )
    }
}

/// Either the underlying port's failure or this fence's refusal.
#[derive(Debug)]
pub enum FencedError<E> {
    Port(E),
    Refused(FenceRefusal),
}

impl<E: std::fmt::Display> std::fmt::Display for FencedError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Port(error) => write!(formatter, "{error}"),
            Self::Refused(refusal) => write!(formatter, "{refusal}"),
        }
    }
}

/// The PR inventory, behind its generation fence.
///
/// The inventory is refreshed from PTY output observation, which a draining owner
/// still produces — and it is a whole-snapshot document, so two writers would lose
/// an update in exactly the way the runtime shards were split to avoid.
///
/// The decision this fence encodes is that a draining generation *does not write
/// the inventory at all*: the observation it holds belongs to terminals that are
/// ending, and the active generation's own refresh recomputes the sessions it
/// still owns. Nothing has to be deferred to an outbox, so the cache the active
/// single writer keeps is never invalidated by somebody else's write.
///
/// Reads stay open to every role: hydrating a cache observes state, and observing
/// cannot lose an update.
pub struct FencedPrInventory<P> {
    port: P,
    role: GenerationRole,
}

impl<P> FencedPrInventory<P> {
    /// Bind `port` to the role of the process that holds it.
    pub const fn new(port: P, role: GenerationRole) -> Self {
        Self { port, role }
    }

    /// Whether this process is the inventory's single writer.
    #[must_use]
    pub fn writable(&self) -> bool {
        shared_write_verdict(SharedWriter::PrInventory, self.role) == WriteVerdict::Allowed
    }
}

impl<P: PrInventoryPort> PrInventoryPort for FencedPrInventory<P> {
    type Error = FencedError<P::Error>;

    fn load(&self) -> Result<BTreeMap<SessionId, PrInventory>, Self::Error> {
        self.port.load().map_err(FencedError::Port)
    }

    fn save(&self, sessions: &BTreeMap<SessionId, PrInventory>) -> Result<(), Self::Error> {
        if !self.writable() {
            return Err(FencedError::Refused(FenceRefusal {
                writer: SharedWriter::PrInventory,
                role: self.role,
            }));
        }
        self.port.save(sessions).map_err(FencedError::Port)
    }
}

#[cfg(test)]
mod tests;

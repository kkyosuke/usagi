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

use std::sync::atomic::{AtomicU8, Ordering};

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

/// The role this process holds right now, readable by the workers that write
/// shared documents.
///
/// The verdict is only worth having if the code that writes actually asks for it,
/// and the PR inventory is written by a background worker on the PTY output path —
/// far away from the lifecycle code that knows the role. Re-reading the durable
/// registry per write would put a locked file read into that hot path (#555), so
/// the role is published once, where it changes, and read cheaply where it is
/// enforced.
#[derive(Debug)]
pub struct SharedRole(AtomicU8);

impl SharedRole {
    /// The role of a process that has just become the active generation.
    #[must_use]
    pub fn active() -> Self {
        Self(AtomicU8::new(role_code(GenerationRole::Active)))
    }

    /// Publish the role this process now holds.
    pub fn set(&self, role: GenerationRole) {
        self.0.store(role_code(role), Ordering::SeqCst);
    }

    /// The role this process holds.
    #[must_use]
    pub fn get(&self) -> GenerationRole {
        ROLES[usize::from(self.0.load(Ordering::SeqCst) & ROLE_MASK)]
    }

    /// What this process may do to `writer` right now.
    #[must_use]
    pub fn verdict(&self, writer: SharedWriter) -> WriteVerdict {
        shared_write_verdict(writer, self.get())
    }

    /// Whether this process is the single writer of `writer` right now.
    #[must_use]
    pub fn may_write(&self, writer: SharedWriter) -> bool {
        self.verdict(writer) == WriteVerdict::Allowed
    }
}

/// The stored codes, in the order [`role_code`] assigns them. Decoding is a
/// lookup rather than a match with a fallback arm, so there is no "impossible"
/// branch to reason about at all.
const ROLES: [GenerationRole; 4] = [
    GenerationRole::Active,
    GenerationRole::Draining,
    GenerationRole::Standby,
    GenerationRole::Retired,
];

const ROLE_MASK: u8 = 0b11;

const fn role_code(role: GenerationRole) -> u8 {
    match role {
        GenerationRole::Active => 0,
        GenerationRole::Draining => 1,
        GenerationRole::Standby => 2,
        GenerationRole::Retired => 3,
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

#[cfg(test)]
mod tests;

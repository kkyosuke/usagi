//! Owner-generation runtime shards and the global resource allocator.
//!
//! [`super::authority`] decides *which generation may act*. This module decides
//! *what each generation may own*, which is the other half of running two daemon
//! processes at once: while a draining owner still reaps its children, a new
//! active owner is already spawning. Two processes cannot whole-save the same
//! snapshot without losing an update, so the durable state is split in two:
//!
//! | piece | question it answers |
//! |---|---|
//! | [`shard`] | what does *this* generation own, and may it be written by *this* process? |
//! | [`allocator`] | across every retained generation: who holds capacity, and what did each producer operation already decide? |
//! | [`identity`] | is this child the exact OS process this owner spawned? |
//! | [`launch`] | claim → reserve → spawn → final, with one named boundary per crash point |
//! | [`drain`] | how a draining owner's exit reaches the active consumer exactly once |
//! | [`retention`] | how the operation ledger stays bounded without ever replaying a wrong answer |
//! | [`migration`] | can the legacy single-writer stores be adopted, or must the rollover refuse? |
//! | [`fence`] | which other shared writers a draining process must not whole-save |
//! | [`durable`] | how the shipping Agent and terminal stores are carried by all of the above |
//!
//! The split is deliberate: a shard has exactly one writer (its owner
//! generation), so the owner never needs to merge. The allocator is shared, so
//! every write to it is a compare-and-swap through [`CasStore`]. Nothing here
//! touches a filesystem — durability is the [`CasFile`] seam and the real
//! adapters live in [`crate::infrastructure::resource_store`].
//!
//! The contract is documented in
//! [5. daemon](../../../../../document/05-daemon.md#owner-generation-runtime-shard-と-global-resource-allocator).

pub mod allocator;
pub mod drain;
pub mod durable;
pub mod fence;
pub mod identity;
pub mod launch;
pub mod migration;
pub mod retention;
pub mod shard;

#[cfg(test)]
mod fixture;

use std::fmt;
use std::io;
use std::marker::PhantomData;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A typed refusal from either durable object. Every variant is effect zero: the
/// document the caller read is left exactly as it was, and no spawn, signal, or
/// capacity release is inferred from a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceError {
    /// The stored schema is not the one this build understands.
    UnknownSchema,
    /// The stored bytes are not a document, or the document contradicts itself.
    Corrupt,
    /// Another writer committed since this writer read the document.
    StaleRevision,
    /// This document belongs to a different owner generation.
    ForeignOwner,
    /// The resource kind's own capacity pool is full across every retained
    /// generation. Pools are never implicitly summed.
    CapacityExhausted,
    /// The same producer operation was already accepted for a different
    /// canonical intent.
    OperationConflict,
    /// The operation is older than the durable expiry watermark, or its full
    /// outcome was already compacted into a tombstone.
    OperationExpired,
    /// The ledger reached a hard retention cap with no safe collection
    /// candidate, so fresh admission is refused instead of evicting a record.
    RetentionBackpressure,
    /// No record of this operation exists where one was required.
    UnknownOperation,
    /// No record of this resource exists where one was required.
    UnknownResource,
    /// The resource is already recorded.
    DuplicateResource,
    /// The record exists but is owned by another generation.
    WrongOwner,
    /// The record exists but is not in the state this transition requires.
    WrongState,
    /// Exactly one side of a two-object write survived, so ownership cannot be
    /// proved. Nothing is spawned, signalled, or released.
    OwnershipUnknown,
    /// The child's recorded start identity is not OS-verifiable, so it can never
    /// be used as spawn authority.
    IdentityUnverifiable,
    /// The writer lease was sealed against revisions that have since moved.
    SealedElsewhere,
    /// The generation still owns live state, so it cannot be collected.
    NotCollectable,
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownSchema => "runtime document schema is not supported",
            Self::Corrupt => "runtime document is corrupt",
            Self::StaleRevision => "runtime document changed under this writer",
            Self::ForeignOwner => "runtime shard belongs to another generation",
            Self::CapacityExhausted => "resource pool capacity is exhausted",
            Self::OperationConflict => "operation id was accepted for a different intent",
            Self::OperationExpired => "operation id is expired",
            Self::RetentionBackpressure => "operation ledger is full and cannot be collected",
            Self::UnknownOperation => "operation is not recorded",
            Self::UnknownResource => "resource is not recorded",
            Self::DuplicateResource => "resource is already recorded",
            Self::WrongOwner => "resource is owned by another generation",
            Self::WrongState => "resource is not in the required state",
            Self::OwnershipUnknown => "resource ownership cannot be proved",
            Self::IdentityUnverifiable => "child process identity is not verifiable",
            Self::SealedElsewhere => "sealed revision moved before the writer opened",
            Self::NotCollectable => "generation still owns live state",
        })
    }
}

impl std::error::Error for ResourceError {}

/// Either a typed refusal or a durable-store failure. Both fail closed; they are
/// separated so a caller can tell "refused" from "unavailable".
#[derive(Debug)]
pub enum ResourceFailure {
    Refused(ResourceError),
    Io(io::Error),
}

impl From<ResourceError> for ResourceFailure {
    fn from(error: ResourceError) -> Self {
        Self::Refused(error)
    }
}

impl From<io::Error> for ResourceFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for ResourceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "runtime document store failed: {error}"),
        }
    }
}

impl std::error::Error for ResourceFailure {}

impl ResourceFailure {
    /// The typed refusal, when this failure is one.
    #[must_use]
    pub fn refusal(&self) -> Option<ResourceError> {
        match self {
            Self::Refused(error) => Some(*error),
            Self::Io(_) => None,
        }
    }
}

/// The durable byte seam every document is read and written through.
///
/// The real adapter serializes both operations under one cross-process lock and
/// replaces the document by atomic rename, so the comparison and the write are
/// one transaction. Tests inject an in-memory fake and drive every transition
/// without a filesystem.
pub trait CasFile {
    /// Read the document's bytes, or `None` when it does not exist yet.
    ///
    /// # Errors
    /// Returns an error when the document exists but cannot be read.
    fn read(&self) -> io::Result<Option<String>>;

    /// Replace the document only while its bytes still equal `expected`
    /// (`None` meaning "still absent").
    ///
    /// # Errors
    /// Returns an error when the document cannot be inspected or replaced.
    /// A failed comparison is `Ok(false)`, not an error.
    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool>;
}

impl CasFile for Box<dyn CasFile + Send> {
    fn read(&self) -> io::Result<Option<String>> {
        self.as_ref().read()
    }

    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool> {
        self.as_ref().compare_and_write(expected, contents)
    }
}

/// A durable document that can be compare-and-swapped.
pub trait CasDocument: Clone + PartialEq + Serialize + DeserializeOwned {
    /// The document-wide revision every commit advances by exactly one.
    fn revision(&self) -> u64;
    /// Advance the revision by one.
    fn bump(&mut self);
    /// Fail closed on anything this build must not act on.
    ///
    /// # Errors
    /// Returns the first violated invariant.
    fn validate(&self) -> Result<(), ResourceError>;
}

/// A document together with the exact bytes it was read from. Committing needs
/// this pair, which is what makes every write a compare-and-swap.
#[derive(Debug, Clone)]
pub struct CasSnapshot<D> {
    document: D,
    observed: Option<String>,
}

impl<D: CasDocument> CasSnapshot<D> {
    /// The document as read.
    #[must_use]
    pub fn document(&self) -> &D {
        &self.document
    }

    /// A mutable copy to stage a transition on.
    #[must_use]
    pub fn to_document(&self) -> D {
        self.document.clone()
    }

    /// The exact bytes this snapshot was read from, so a caller can prove a
    /// refusal left the durable object untouched.
    #[must_use]
    pub fn observed(&self) -> Option<&str> {
        self.observed.as_deref()
    }
}

/// A compare-and-swapped document over a [`CasFile`].
///
/// The seam is a trait object rather than a type parameter: the production
/// adapters and the in-memory fakes then share exactly one compiled copy of the
/// swap protocol, so nothing about which store a caller binds can change the
/// code that runs.
pub struct CasStore<D> {
    file: Box<dyn CasFile + Send>,
    document: PhantomData<D>,
}

impl<D: CasDocument> CasStore<D> {
    /// Bind a store to its byte seam.
    pub fn new(file: impl CasFile + Send + 'static) -> Self {
        Self {
            file: Box::new(file),
            document: PhantomData,
        }
    }

    /// Read and validate the document, using `absent` when none exists yet.
    ///
    /// # Errors
    /// Returns [`ResourceError::Corrupt`] or the document's own validation
    /// refusal for bytes this build must not act on, or the store's read error.
    pub fn load(&self, absent: impl FnOnce() -> D) -> Result<CasSnapshot<D>, ResourceFailure> {
        let Some(observed) = self.file.read()? else {
            let document = absent();
            document.validate()?;
            return Ok(CasSnapshot {
                document,
                observed: None,
            });
        };
        let document: D = serde_json::from_str(&observed).map_err(|_| ResourceError::Corrupt)?;
        document.validate()?;
        Ok(CasSnapshot {
            document,
            observed: Some(observed),
        })
    }

    /// Commit `next` against the exact bytes `snapshot` was read from.
    ///
    /// # Errors
    /// Returns [`ResourceError::StaleRevision`] when another writer committed
    /// first or when `next` does not advance the revision by exactly one, the
    /// document's validation refusal, or the store's error.
    pub fn commit(
        &self,
        snapshot: &CasSnapshot<D>,
        next: D,
    ) -> Result<CasSnapshot<D>, ResourceFailure> {
        if next.revision() != snapshot.document.revision() + 1 {
            return Err(ResourceError::StaleRevision.into());
        }
        next.validate()?;
        let contents = serde_json::to_string(&next).map_err(|_| ResourceError::Corrupt)?;
        if !self
            .file
            .compare_and_write(snapshot.observed.as_deref(), &contents)?
        {
            return Err(ResourceError::StaleRevision.into());
        }
        Ok(CasSnapshot {
            document: next,
            observed: Some(contents),
        })
    }

    /// Load, apply `change`, and commit in one compare-and-swap. `change` runs on
    /// a copy, so a refusal commits nothing and a converged retry writes nothing
    /// at all.
    ///
    /// # Errors
    /// Returns `change`'s refusal, or any [`load`](Self::load) /
    /// [`commit`](Self::commit) failure.
    pub fn update<T>(
        &self,
        absent: impl FnOnce() -> D,
        change: impl FnOnce(&mut D) -> Result<T, ResourceError>,
    ) -> Result<(T, CasSnapshot<D>), ResourceFailure> {
        let snapshot = self.load(absent)?;
        let mut next = snapshot.to_document();
        let value = change(&mut next)?;
        if next == snapshot.document {
            return Ok((value, snapshot));
        }
        next.bump();
        let committed = self.commit(&snapshot, next)?;
        Ok((value, committed))
    }
}

#[cfg(test)]
mod tests;

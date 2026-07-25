//! The durable cross-process generation registry.
//!
//! The registry is one JSON document holding every retained generation, its
//! role, endpoint, process identity, expected/actual build artifact, and the
//! in-flight handoff. It is written through a compare-and-swap seam: a writer
//! commits only against the exact bytes it read, so a stale writer loses
//! instead of overwriting a newer authority.
//!
//! Refusals are effect zero. An unknown schema, a corrupt record, a stale
//! writer, or an invalid role transition leaves the durable document exactly as
//! it was — the caller never falls back to a weaker authority.

use std::collections::BTreeSet;
use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, OperationId, build_artifact_decision,
};

use crate::usecase::generation::{GenerationRole, ProcessIdentity};

/// The only registry schema this build understands. An unknown value fails
/// closed rather than being migrated by guesswork.
pub const REGISTRY_SCHEMA: &str = "usagi-generation-registry-v1";

/// The maximum number of simultaneously retained (non-retired) generations.
/// Two is exactly enough for one draining owner plus one active successor, so a
/// repeated rollover cannot multiply daemon processes.
pub const DEFAULT_GENERATION_LIMIT: usize = crate::usecase::generation::DEFAULT_GENERATION_LIMIT;

/// The durable phase of a handoff. The write order and what each phase implies
/// after a crash are documented in [`crate::usecase::authority::handoff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffPhase {
    /// The intent is durable but no authority has moved. Nothing is observable
    /// by a client yet, so this phase may be abandoned.
    Preparing,
    /// Roles and `current` have moved in the registry. The commit is
    /// observable, so it is only ever rolled forward.
    Committed,
}

/// One in-flight authority handoff, keyed by a producer-issued operation id so
/// concurrent and repeated rollovers converge on a single outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffRecord {
    pub operation: OperationId,
    /// The generation losing `active`. Absent for the first activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<DaemonGeneration>,
    pub to: DaemonGeneration,
    /// The endpoint that must become `current`, carried so recovery can
    /// republish the locator without trusting a second lookup.
    pub endpoint: String,
    pub phase: HandoffPhase,
}

/// One retained generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationEntry {
    pub generation: DaemonGeneration,
    pub role: GenerationRole,
    /// The endpoint spelled exactly as the current locator spells it.
    pub endpoint: String,
    pub process: ProcessIdentity,
    /// The artifact this generation was admitted for. A cross-process standby
    /// must name a known artifact before it consumes a generation slot.
    pub expected_build: BuildIdentity,
    /// The artifact its own `ServerHello` advertised after readiness. Set only
    /// on an exact match with `expected_build`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_build: Option<BuildIdentity>,
    /// Bumped on every role change so a late writer can detect that the
    /// generation it holds a lease for has moved on.
    pub revision: u64,
}

impl GenerationEntry {
    /// Whether post-readiness identity has been proved for this entry.
    #[must_use]
    pub fn is_build_verified(&self) -> bool {
        self.verified_build
            .as_ref()
            .is_some_and(|actual| actual.same_artifact(&self.expected_build))
    }
}

/// The whole registry. `revision` covers the document, not one entry, so every
/// commit is a compare-and-swap against a single monotonic counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryDocument {
    pub schema: String,
    pub revision: u64,
    /// The generation whose endpoint the current locator names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<DaemonGeneration>,
    pub generations: Vec<GenerationEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<HandoffRecord>,
    /// The last handoff that reached its terminal outcome. A retried rollover
    /// carrying this operation id converges here instead of starting a second
    /// process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_operation: Option<OperationId>,
}

impl Default for RegistryDocument {
    fn default() -> Self {
        Self {
            schema: REGISTRY_SCHEMA.to_owned(),
            revision: 0,
            current: None,
            generations: Vec::new(),
            handoff: None,
            completed_operation: None,
        }
    }
}

/// A refusal. Every variant leaves the durable document untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    /// The stored schema is not [`REGISTRY_SCHEMA`].
    UnknownSchema,
    /// The stored bytes are not a registry document, or the document
    /// contradicts itself (duplicate generation, dangling handoff target).
    Corrupt,
    /// Another writer committed since this writer read the document.
    StaleRevision,
    /// The generation is already registered.
    DuplicateGeneration,
    /// The generation is not in the registry.
    UnknownGeneration,
    /// Registering would retain more than the configured limit.
    GenerationLimit,
    /// The requested role change is not in the allowed transition table.
    InvalidTransition,
    /// More than one generation claims `active`, or `current` does not name the
    /// single active generation.
    MultipleActive,
    /// An identity that cannot be compared as a real artifact.
    BuildIdentityUnknown,
    /// A known identity that is not the expected artifact.
    BuildMismatch,
    /// A different operation already owns the in-flight handoff.
    HandoffInProgress,
    /// No handoff is in flight, or the in-flight handoff is a different
    /// operation than the caller's.
    UnknownOperation,
    /// The handoff is not in the phase this step requires.
    WrongPhase,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownSchema => "generation registry schema is not supported",
            Self::Corrupt => "generation registry is corrupt",
            Self::StaleRevision => "generation registry changed under this writer",
            Self::DuplicateGeneration => "generation is already registered",
            Self::UnknownGeneration => "generation is not registered",
            Self::GenerationLimit => "generation limit reached",
            Self::InvalidTransition => "generation role transition is not allowed",
            Self::MultipleActive => "registry does not hold exactly one active generation",
            Self::BuildIdentityUnknown => "build artifact identity is unknown",
            Self::BuildMismatch => "build artifact identity does not match",
            Self::HandoffInProgress => "another handoff operation is in flight",
            Self::UnknownOperation => "handoff operation is not in flight",
            Self::WrongPhase => "handoff is not in the required phase",
        })
    }
}

impl std::error::Error for RegistryError {}

/// Either a typed refusal or a durable-store failure. Both are fail closed;
/// they are separated so a caller can distinguish "refused" from "unavailable".
#[derive(Debug)]
pub enum RegistryFailure {
    Refused(RegistryError),
    Io(io::Error),
}

impl From<RegistryError> for RegistryFailure {
    fn from(error: RegistryError) -> Self {
        Self::Refused(error)
    }
}

impl From<io::Error> for RegistryFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for RegistryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "generation registry store failed: {error}"),
        }
    }
}

impl std::error::Error for RegistryFailure {}

impl RegistryFailure {
    /// The typed refusal, when this failure is one.
    #[must_use]
    pub fn refusal(&self) -> Option<RegistryError> {
        match self {
            Self::Refused(error) => Some(*error),
            Self::Io(_) => None,
        }
    }
}

/// The durable byte seam the registry is read and written through.
///
/// The real adapter serializes both operations with one cross-process lock and
/// replaces the document by atomic rename. Tests inject an in-memory fake, so
/// every state transition above is exercised without touching a filesystem.
pub trait RegistryFile {
    /// Read the document's bytes, or `None` when no registry exists yet.
    ///
    /// # Errors
    /// Returns an error when the document exists but cannot be read.
    fn read(&self) -> io::Result<Option<String>>;

    /// Replace the document only when its bytes still equal `expected`
    /// (`None` meaning "still absent"). The comparison and the replacement must
    /// be one transaction relative to every other writer.
    ///
    /// # Errors
    /// Returns an error when the document cannot be inspected or replaced.
    /// Returns `Ok(false)` — not an error — when the comparison failed.
    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool>;
}

/// A document together with the exact bytes it was read from. Committing
/// requires this pair, which is what makes every write a compare-and-swap.
#[derive(Debug, Clone)]
pub struct RegistrySnapshot {
    document: RegistryDocument,
    observed: Option<String>,
}

impl RegistrySnapshot {
    /// The document as read.
    #[must_use]
    pub fn document(&self) -> &RegistryDocument {
        &self.document
    }

    /// A mutable copy to stage a transition on.
    #[must_use]
    pub fn to_document(&self) -> RegistryDocument {
        self.document.clone()
    }
}

/// The durable registry over a [`RegistryFile`].
pub struct GenerationRegistry<F> {
    file: F,
    limit: usize,
}

impl<F: RegistryFile> GenerationRegistry<F> {
    /// Build a registry retaining at most `limit` non-retired generations.
    pub fn new(file: F, limit: usize) -> Self {
        Self { file, limit }
    }

    /// The configured retention limit.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Read and validate the registry. An absent document is an empty registry,
    /// which is the only "missing" case that is not a failure.
    ///
    /// # Errors
    /// Returns [`RegistryError::UnknownSchema`] or [`RegistryError::Corrupt`]
    /// for bytes this build must not act on, or the store's read error.
    pub fn load(&self) -> Result<RegistrySnapshot, RegistryFailure> {
        let Some(observed) = self.file.read()? else {
            return Ok(RegistrySnapshot {
                document: RegistryDocument::default(),
                observed: None,
            });
        };
        let document: RegistryDocument =
            serde_json::from_str(&observed).map_err(|_| RegistryError::Corrupt)?;
        document.validate(self.limit)?;
        Ok(RegistrySnapshot {
            document,
            observed: Some(observed),
        })
    }

    /// Commit `next` against the exact bytes `snapshot` was read from.
    ///
    /// # Errors
    /// Returns [`RegistryError::StaleRevision`] when another writer committed
    /// first or when `next` does not advance the revision by exactly one, the
    /// validation refusal for an inconsistent document, or the store's error.
    pub fn commit(
        &self,
        snapshot: &RegistrySnapshot,
        next: RegistryDocument,
    ) -> Result<RegistrySnapshot, RegistryFailure> {
        if next.revision != snapshot.document.revision + 1 {
            return Err(RegistryError::StaleRevision.into());
        }
        next.validate(self.limit)?;
        let contents = serde_json::to_string(&next).map_err(|_| RegistryError::Corrupt)?;
        if !self
            .file
            .compare_and_write(snapshot.observed.as_deref(), &contents)?
        {
            return Err(RegistryError::StaleRevision.into());
        }
        Ok(RegistrySnapshot {
            document: next,
            observed: Some(contents),
        })
    }

    /// Load, apply `change`, and commit in one compare-and-swap. `change` runs
    /// on a copy: a refusal commits nothing.
    ///
    /// # Errors
    /// Returns `change`'s refusal, or any [`load`](Self::load) /
    /// [`commit`](Self::commit) failure.
    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut RegistryDocument) -> Result<T, RegistryError>,
    ) -> Result<(T, RegistrySnapshot), RegistryFailure> {
        let snapshot = self.load()?;
        let mut next = snapshot.to_document();
        let value = change(&mut next)?;
        if next == snapshot.document {
            // A converged retry writes nothing at all, so a replayed operation
            // cannot be told apart from the original by its durable effect.
            return Ok((value, snapshot));
        }
        next.revision += 1;
        let committed = self.commit(&snapshot, next)?;
        Ok((value, committed))
    }
}

impl RegistryDocument {
    /// Fail closed on anything this build must not act on.
    ///
    /// # Errors
    /// Returns the first violated invariant.
    pub fn validate(&self, limit: usize) -> Result<(), RegistryError> {
        if self.schema != REGISTRY_SCHEMA {
            return Err(RegistryError::UnknownSchema);
        }
        let mut seen = BTreeSet::new();
        for entry in &self.generations {
            if !seen.insert(entry.generation.as_str()) {
                return Err(RegistryError::Corrupt);
            }
        }
        if self.retained() > limit {
            return Err(RegistryError::GenerationLimit);
        }
        let actives: Vec<_> = self
            .generations
            .iter()
            .filter(|entry| entry.role == GenerationRole::Active)
            .map(|entry| entry.generation)
            .collect();
        match (actives.as_slice(), self.current) {
            ([], None) => {}
            ([active], Some(current)) if *active == current => {}
            _ => return Err(RegistryError::MultipleActive),
        }
        if let Some(handoff) = &self.handoff {
            if self.entry(handoff.to).is_none() {
                return Err(RegistryError::Corrupt);
            }
            if handoff.from.is_some_and(|from| self.entry(from).is_none()) {
                return Err(RegistryError::Corrupt);
            }
        }
        Ok(())
    }

    /// The entry for `generation`, if it is retained.
    #[must_use]
    pub fn entry(&self, generation: DaemonGeneration) -> Option<&GenerationEntry> {
        self.generations
            .iter()
            .find(|entry| entry.generation == generation)
    }

    /// The role of `generation`, if it is retained.
    #[must_use]
    pub fn role(&self, generation: DaemonGeneration) -> Option<GenerationRole> {
        self.entry(generation).map(|entry| entry.role)
    }

    /// The single active generation's entry, when there is one.
    #[must_use]
    pub fn active(&self) -> Option<&GenerationEntry> {
        self.current.and_then(|current| self.entry(current))
    }

    /// The number of generations still holding a process.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.generations
            .iter()
            .filter(|entry| entry.role != GenerationRole::Retired)
            .count()
    }

    /// Admit a private standby for an exact expected artifact.
    ///
    /// Re-registering the identical standby is idempotent, so a retried
    /// rollover does not consume a second slot. An unknown identity is refused
    /// before a slot is taken at all.
    ///
    /// # Errors
    /// Returns [`RegistryError::BuildIdentityUnknown`],
    /// [`RegistryError::DuplicateGeneration`], or
    /// [`RegistryError::GenerationLimit`].
    pub fn register_standby(
        &mut self,
        limit: usize,
        generation: DaemonGeneration,
        endpoint: impl Into<String>,
        process: ProcessIdentity,
        expected_build: BuildIdentity,
    ) -> Result<(), RegistryError> {
        if !expected_build.is_known() {
            return Err(RegistryError::BuildIdentityUnknown);
        }
        let candidate = GenerationEntry {
            generation,
            role: GenerationRole::Standby,
            endpoint: endpoint.into(),
            process,
            expected_build,
            verified_build: None,
            revision: 1,
        };
        if let Some(existing) = self.entry(generation) {
            // Identity, not the whole record: a standby that already reached
            // verified readiness must still accept its own re-registration.
            let same = existing.role == GenerationRole::Standby
                && existing.endpoint == candidate.endpoint
                && existing.process == candidate.process
                && existing.expected_build == candidate.expected_build;
            return if same {
                Ok(())
            } else {
                Err(RegistryError::DuplicateGeneration)
            };
        }
        if self.retained() >= limit {
            // `validate` checks this again at commit time; refusing here keeps
            // the caller's error specific instead of surfacing a limit
            // violation as a document validation failure.
            return Err(RegistryError::GenerationLimit);
        }
        self.generations.push(candidate);
        Ok(())
    }

    /// Record that a standby's own `ServerHello` advertised exactly the
    /// artifact it was admitted for. This is effect free: a mismatch leaves the
    /// active generation and the candidate's role untouched.
    ///
    /// # Errors
    /// Returns [`RegistryError::UnknownGeneration`],
    /// [`RegistryError::InvalidTransition`] when the candidate is not a
    /// standby, [`RegistryError::BuildIdentityUnknown`], or
    /// [`RegistryError::BuildMismatch`].
    pub fn verify_standby_build(
        &mut self,
        generation: DaemonGeneration,
        actual: &BuildIdentity,
    ) -> Result<(), RegistryError> {
        let index = self.index_of(generation)?;
        let entry = &mut self.generations[index];
        if entry.role != GenerationRole::Standby {
            return Err(RegistryError::InvalidTransition);
        }
        if !entry.expected_build.is_known() || !actual.is_known() {
            return Err(RegistryError::BuildIdentityUnknown);
        }
        if build_artifact_decision(actual, &entry.expected_build, false)
            != BuildArtifactDecision::Reuse
        {
            return Err(RegistryError::BuildMismatch);
        }
        entry.verified_build = Some(actual.clone());
        entry.revision += 1;
        Ok(())
    }

    /// Move `generation` to `role` through the allowed transition table.
    ///
    /// # Errors
    /// Returns [`RegistryError::UnknownGeneration`] or
    /// [`RegistryError::InvalidTransition`].
    pub fn transition(
        &mut self,
        generation: DaemonGeneration,
        role: GenerationRole,
    ) -> Result<(), RegistryError> {
        let index = self.index_of(generation)?;
        let entry = &mut self.generations[index];
        if !transition_allowed(entry.role, role) {
            return Err(RegistryError::InvalidTransition);
        }
        entry.role = role;
        entry.revision += 1;
        if self.current == Some(generation) && role != GenerationRole::Active {
            self.current = None;
        }
        Ok(())
    }

    fn index_of(&self, generation: DaemonGeneration) -> Result<usize, RegistryError> {
        self.generations
            .iter()
            .position(|entry| entry.generation == generation)
            .ok_or(RegistryError::UnknownGeneration)
    }
}

/// The allowed role transitions. A generation only ever moves toward
/// retirement, so no path resurrects an authority a client already stopped
/// seeing.
///
/// ```text
/// standby ──▶ active ──▶ draining ──▶ retired
///    │           │                       ▲
///    └───────────┴───────────────────────┘
/// ```
#[must_use]
pub fn transition_allowed(from: GenerationRole, to: GenerationRole) -> bool {
    matches!(
        (from, to),
        (
            GenerationRole::Standby,
            GenerationRole::Active | GenerationRole::Retired
        ) | (
            GenerationRole::Active,
            GenerationRole::Draining | GenerationRole::Retired
        ) | (GenerationRole::Draining, GenerationRole::Retired)
    )
}

#[cfg(test)]
mod tests;

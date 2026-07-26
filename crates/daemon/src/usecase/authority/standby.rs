//! Side-effect-free standby readiness.
//!
//! A standby proves it can serve *before* it is allowed to become `current`,
//! and proving it must change nothing: no locator write, no runtime store
//! reconcile or save, no supervisor tick, no worker start, no spawn. The only
//! durable writes on this path are the two registry compare-and-swaps that
//! admit the standby and record its verified identity.
//!
//! What readiness proves is exact: the endpoint answers, the answering peer is
//! the generation that was registered, it advertises the capabilities a handoff
//! needs, and its own `ServerHello` artifact is byte-for-byte the artifact the
//! standby was admitted for. An unknown identity on either side is not promoted
//! to a match by a version/target agreement, and any refusal leaves the old
//! active generation and the current locator exactly as they were.
//!
//! Two decisions bracket that readiness run, and both are pure:
//!
//! * [`admissible_active`] answers "may this process become a standby here at
//!   all", *before* it binds anything. A standby is the only daemon that runs in
//!   a data directory it does not own, so it may only exist next to an active
//!   generation the registry itself names. That is what fails a mixed build
//!   closed: an old `serve` that never registers is a live owner the registry
//!   cannot account for, so no standby joins it.
//! * [`evaluate_custody`] answers "is this process still that standby". A
//!   standby holds no lock and no lifecycle record, so its registry entry *is*
//!   its custody: recovery that fails an abandoned authority closed retires
//!   every generation, and the standby it retired has to notice and exit rather
//!   than linger as an orphan.

use std::fmt;
use std::io;

use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, ServerHello, build_artifact_decision,
};

use crate::usecase::authority::registry::{
    GenerationRegistry, REGISTRY_SCHEMA, RegistryDocument, RegistryError, RegistryFailure,
    RegistrySnapshot,
};
use crate::usecase::generation::{GenerationRole, ProcessIdentity};

/// The capability a peer advertises when its `ServerHello` carries a canonical
/// build artifact identity. Without it the peer is an older build that cannot
/// be compared, which fails safe.
pub const BUILD_ARTIFACT_CAPABILITY: &str = "build.artifact.v1";

/// The capability a peer advertises when it participates in the cross-process
/// generation registry and honours per-request role admission.
pub const GENERATION_HANDOFF_CAPABILITY: &str = "daemon.generation-handoff.v1";

/// Why a standby was not admitted to authority. Every variant keeps the old
/// active generation and the current locator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessRefusal {
    /// The peer at the private endpoint is not the registered generation.
    GenerationMismatch,
    /// One side cannot identify its artifact.
    IdentityUnknown,
    /// A known artifact that is not the expected one — including the same
    /// version and target built from a different source tree.
    IdentityMismatch,
    /// The peer does not advertise a capability the handoff requires.
    UnsupportedCapability,
}

impl fmt::Display for ReadinessRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::GenerationMismatch => "standby endpoint is served by another generation",
            Self::IdentityUnknown => "standby build artifact identity is unknown",
            Self::IdentityMismatch => "standby build artifact identity does not match",
            Self::UnsupportedCapability => "standby lacks a required handoff capability",
        })
    }
}

impl std::error::Error for ReadinessRefusal {}

/// A readiness run failed to reach a verified standby.
#[derive(Debug)]
pub enum StandbyFailure {
    /// The peer was reached but refused.
    Refused(ReadinessRefusal),
    /// The registry refused or was unavailable.
    Registry(RegistryFailure),
    /// The private endpoint could not be probed.
    Probe(io::Error),
}

impl From<RegistryFailure> for StandbyFailure {
    fn from(failure: RegistryFailure) -> Self {
        Self::Registry(failure)
    }
}

impl From<RegistryError> for StandbyFailure {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error.into())
    }
}

impl From<ReadinessRefusal> for StandbyFailure {
    fn from(refusal: ReadinessRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl fmt::Display for StandbyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "{refusal}"),
            Self::Registry(failure) => write!(f, "{failure}"),
            Self::Probe(error) => write!(f, "standby endpoint probe failed: {error}"),
        }
    }
}

impl std::error::Error for StandbyFailure {}

/// A read-only handshake against a standby's private endpoint.
///
/// The implementation connects, completes a hello, and closes. It issues no
/// request that could mutate the peer, which is what keeps readiness effect
/// free on *both* sides of the connection.
pub trait StandbyProbe {
    /// Complete a hello against `endpoint` and return the peer's reply.
    ///
    /// # Errors
    /// Returns an error when the endpoint cannot be reached or the handshake
    /// does not complete.
    fn hello(&self, endpoint: &str) -> io::Result<ServerHello>;
}

/// What the data directory says about the daemon that owns it right now.
///
/// The pair is deliberately the lifecycle record *and* the OS observation of it,
/// not the record alone: a record naming a dead or PID-reused process is exactly
/// the state a crashed active leaves, and standing by for it would produce a
/// standby whose successor never existed.
pub struct ActiveOwner<'a> {
    /// `daemon.json`, when a record exists at all.
    pub record: Option<&'a DaemonRecord>,
    /// What the OS says about that record's process.
    pub observation: DaemonProcessObservation,
}

/// Why a process may not become a standby in this data directory. Every variant
/// is refused before anything is bound or written, so a refusal leaves the
/// registry, the locator, and the active daemon exactly as they were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandbyStartRefusal {
    /// No registry exists, so no daemon has ever taken authority here. There is
    /// nothing to stand by for.
    NoGenerationRegistry,
    /// The registry is not the schema this build writes.
    RegistrySchemaUnsupported,
    /// No live daemon owns this data directory. A standby is not a way to start
    /// serving; the first daemon activates instead
    /// ([`super::activation::claim_authority`]).
    NoLiveOwner,
    /// A live daemon owns this data directory but the registry does not name it
    /// as the active generation. That is the mixed-build case — an old `serve`
    /// holds the directory without registering — and it fails closed: a standby
    /// admitted next to an authority nothing can name could later be handed
    /// authority the old owner never gave up.
    OwnerUnregistered,
    /// A handoff is already in flight. Its two roles are the authority's whole
    /// budget, so a third process may not join in the middle of one.
    HandoffInFlight,
}

impl fmt::Display for StandbyStartRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoGenerationRegistry => {
                "no generation registry exists, so there is no active generation to stand by for"
            }
            Self::RegistrySchemaUnsupported => {
                "the generation registry is not the schema this build writes"
            }
            Self::NoLiveOwner => "no live daemon owns this data directory",
            Self::OwnerUnregistered => {
                "the daemon that owns this data directory is not the registry's active generation"
            }
            Self::HandoffInFlight => "a generation handoff is already in flight",
        })
    }
}

impl std::error::Error for StandbyStartRefusal {}

impl From<StandbyStartRefusal> for io::Error {
    fn from(refusal: StandbyStartRefusal) -> Self {
        Self::other(refusal.to_string())
    }
}

/// The active generation a new standby would stand by for.
///
/// It is proved from two independent objects that must agree: the registry names
/// an active generation, and the lifecycle record names an exactly-observed
/// process with the same identity. Agreement is what makes the answer stronger
/// than either object alone — the registry cannot name a dead authority as live,
/// and a live owner cannot hide from the registry.
///
/// # Errors
/// Returns the [`StandbyStartRefusal`] that keeps this process from binding
/// anything at all.
pub fn admissible_active(
    registry: Option<&RegistryDocument>,
    owner: &ActiveOwner<'_>,
) -> Result<DaemonGeneration, StandbyStartRefusal> {
    let Some(document) = registry else {
        return Err(StandbyStartRefusal::NoGenerationRegistry);
    };
    if document.schema != REGISTRY_SCHEMA {
        return Err(StandbyStartRefusal::RegistrySchemaUnsupported);
    }
    let record = owner
        .record
        .filter(|_| owner.observation == DaemonProcessObservation::Exact)
        .ok_or(StandbyStartRefusal::NoLiveOwner)?;
    if document.handoff.is_some() {
        return Err(StandbyStartRefusal::HandoffInFlight);
    }
    let active = document
        .active()
        .filter(|active| {
            active.process.pid == record.pid
                && Some(&active.process.start_identity) == record.process_start_identity.as_ref()
        })
        .ok_or(StandbyStartRefusal::OwnerUnregistered)?;
    Ok(active.generation)
}

/// Why a standby is no longer the generation it registered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandbyCustodyLoss {
    /// The registry no longer retains this generation at all.
    EntryAbsent,
    /// The entry was retired — by a handoff that failed closed, by recovery
    /// after the active died, or by collection.
    EntryRetired,
    /// The entry names another process, so this one is not that generation.
    EntryReplaced,
}

impl StandbyCustodyLoss {
    /// A stable, log-friendly reason for this loss.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::EntryAbsent => "registry entry is gone",
            Self::EntryRetired => "registry entry is retired",
            Self::EntryReplaced => "registry entry names another process",
        }
    }
}

/// Whether a standby still holds the registry entry it registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandbyCustody {
    /// The entry is still this process's, in a role that has not retired.
    Held,
    /// The entry is gone, retired, or another process's: exit gracefully.
    Lost(StandbyCustodyLoss),
}

/// Decide whether the standby `generation`, running as `process`, still holds
/// its registry entry.
///
/// A role that moved *forward* — a standby that a handoff made active or
/// draining — is still custody: only retirement ends it, because only a retired
/// generation admits nothing at all
/// ([`super::admission::classify`]).
#[must_use]
pub fn evaluate_custody(
    document: &RegistryDocument,
    generation: DaemonGeneration,
    process: &ProcessIdentity,
) -> StandbyCustody {
    let Some(entry) = document.entry(generation) else {
        return StandbyCustody::Lost(StandbyCustodyLoss::EntryAbsent);
    };
    if entry.role == GenerationRole::Retired {
        return StandbyCustody::Lost(StandbyCustodyLoss::EntryRetired);
    }
    if &entry.process != process {
        return StandbyCustody::Lost(StandbyCustodyLoss::EntryReplaced);
    }
    StandbyCustody::Held
}

/// Compare a standby's answer against what it was admitted for.
///
/// # Errors
/// Returns the [`ReadinessRefusal`] that keeps the old authority in place.
pub fn verify_readiness(
    generation: DaemonGeneration,
    expected: &BuildIdentity,
    hello: &ServerHello,
) -> Result<(), ReadinessRefusal> {
    if hello.daemon_generation.0 != generation.as_str() {
        return Err(ReadinessRefusal::GenerationMismatch);
    }
    for required in [BUILD_ARTIFACT_CAPABILITY, GENERATION_HANDOFF_CAPABILITY] {
        if !hello
            .capabilities
            .iter()
            .any(|advertised| advertised == required)
        {
            return Err(ReadinessRefusal::UnsupportedCapability);
        }
    }
    match build_artifact_decision(&hello.build, expected, false) {
        BuildArtifactDecision::Reuse => Ok(()),
        BuildArtifactDecision::Unknown => Err(ReadinessRefusal::IdentityUnknown),
        BuildArtifactDecision::RolloverTrigger | BuildArtifactDecision::ForceReplace => {
            Err(ReadinessRefusal::IdentityMismatch)
        }
    }
}

/// Admit a private standby and bring it to verified readiness.
///
/// The sequence is: register the standby for its expected artifact (registry
/// CAS), probe its private endpoint read-only, compare identities, then record
/// the verified identity (registry CAS). Re-running it for an already verified
/// standby is idempotent.
///
/// The current locator is never read or written here — the caller performs the
/// authority handoff separately, and only after this returns.
///
/// # Errors
/// Returns the registry refusal, the probe's IO error, or the readiness
/// refusal. In every case the active generation and the locator are unchanged.
pub fn prepare_standby(
    registry: &GenerationRegistry,
    probe: &dyn StandbyProbe,
    generation: DaemonGeneration,
    endpoint: &str,
    process: &ProcessIdentity,
    expected_build: &BuildIdentity,
) -> Result<RegistrySnapshot, StandbyFailure> {
    let limit = registry.limit();
    let ((), snapshot) = registry.update(|document| {
        document.register_standby(
            limit,
            generation,
            endpoint,
            process.clone(),
            expected_build.clone(),
        )
    })?;
    if snapshot
        .document()
        .entry(generation)
        .is_some_and(super::registry::GenerationEntry::is_build_verified)
    {
        return Ok(snapshot);
    }
    let hello = probe.hello(endpoint).map_err(StandbyFailure::Probe)?;
    verify_readiness(generation, expected_build, &hello)?;
    let ((), verified) =
        registry.update(|document| document.verify_standby_build(generation, &hello.build))?;
    Ok(verified)
}

#[cfg(test)]
mod tests;

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

use std::fmt;
use std::io;

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, ServerHello, build_artifact_decision,
};

use crate::usecase::authority::registry::{
    GenerationRegistry, RegistryError, RegistryFailure, RegistrySnapshot,
};
use crate::usecase::generation::ProcessIdentity;

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

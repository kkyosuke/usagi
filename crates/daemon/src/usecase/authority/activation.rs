//! Taking the durable registry authority for a serving daemon process.
//!
//! [`super::rollover`] moves authority *between* two live generations. This
//! module is the other entry into the same registry: the one a daemon that is
//! starting from nothing takes. Without it the registry only ever exists in a
//! test, and [`crate::usecase::replacement::seamless_refusal`] can never say
//! anything but "no generation registry" — which is why a rollover has no
//! successor to name in the first place.
//!
//! The write order is the handoff's order with the predecessor removed, so the
//! crash matrix is the same table read with `from = None`:
//!
//! ```text
//! W1  registry CAS   this generation becomes active, current = it
//! W2  locator write  current.json names this generation's endpoint
//! ```
//!
//! | crash boundary | durable state | what the next start does |
//! |---|---|---|
//! | before W1 | no registry entry, no locator | claims authority normally |
//! | W1..W2 | active names a dead process, locator absent | [`super::handoff::plan_recovery`] fails closed: retires the entry, leaves no active |
//! | after W2 | active names a dead process, locator names it | fails closed as above **and** retires the locator |
//!
//! Nothing here is observable to a client until W2, because the locator is the
//! only thing a client connects through. That is what makes a failure between
//! the two writes recoverable by retirement rather than by guesswork: an entry
//! whose process cannot be proved alive is never adopted as an authority.
//!
//! The endpoint must already be bound and answering before W1. A registry entry
//! naming an endpoint nobody is accepting on would be an authority a client can
//! discover and not reach, so binding is the caller's precondition, not a step
//! this module performs.

use std::fmt;
use std::io;

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::BuildIdentity;

use crate::usecase::authority::handoff::{PublishedLocator, RecoveryOutcome};
use crate::usecase::authority::registry::{GenerationRegistry, RegistryFailure, RegistryFile};
use crate::usecase::authority::rollover::{CurrentLocator, HandoffFailure, recover};
use crate::usecase::generation::{ProcessIdentity, ProcessObservation};

/// What a serving process claims authority as.
///
/// The endpoint is the spelling the locator will carry, so it is the caller's
/// own bound endpoint rather than a path this module composes.
pub struct AuthorityClaim<'a> {
    /// The generation this process bound its endpoint under.
    pub generation: DaemonGeneration,
    /// The already-bound, already-accepting endpoint.
    pub endpoint: &'a str,
    /// This process's OS-observed identity, so a later recovery can prove
    /// whether the authority is still alive.
    pub process: &'a ProcessIdentity,
    /// The artifact this process advertises.
    pub build: &'a BuildIdentity,
}

/// What claiming the authority did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityClaimed {
    /// What the preceding reconciliation found. It is reported rather than
    /// swallowed: a start that had to fail an abandoned handoff closed is a
    /// diagnosable event, not a silent one.
    pub recovery: RecoveryOutcome,
}

/// Why a serving process did not take the registry authority. Every variant
/// leaves the caller free to fail its own startup: no locator was published, so
/// no client can have discovered this generation.
#[derive(Debug)]
pub enum ClaimFailure {
    /// Reconciling what a previous incarnation left did not converge.
    Recovery(HandoffFailure),
    /// The registry refused the claim or could not be written.
    Registry(RegistryFailure),
    /// The registry named this generation active, but the locator could not be
    /// published. Recovery retires the entry on the next start.
    Locator(io::Error),
}

impl fmt::Display for ClaimFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recovery(failure) => write!(f, "generation recovery failed: {failure}"),
            Self::Registry(failure) => write!(f, "{failure}"),
            Self::Locator(error) => write!(f, "current locator failed: {error}"),
        }
    }
}

impl std::error::Error for ClaimFailure {}

impl From<ClaimFailure> for io::Error {
    fn from(failure: ClaimFailure) -> Self {
        Self::other(failure.to_string())
    }
}

/// Reconcile what a previous incarnation left, then become the single active
/// generation and publish `current`.
///
/// `observe` supplies exact OS evidence about a recorded process, so a reused
/// PID never becomes proof that an old authority survives.
///
/// # Errors
/// Returns [`ClaimFailure`]. A refusal before the registry commit leaves the
/// registry and the locator exactly as they were; a locator failure after it
/// leaves an entry whose process the next recovery cannot prove alive, which
/// that recovery retires.
pub fn claim_authority<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    locator: &dyn CurrentLocator,
    claim: &AuthorityClaim<'_>,
    observe: &mut dyn FnMut(&ProcessIdentity) -> ProcessObservation,
) -> Result<AuthorityClaimed, ClaimFailure> {
    let recovery = recover(registry, locator, observe).map_err(ClaimFailure::Recovery)?;
    let limit = registry.limit();
    registry
        .update(|document| {
            document.activate_first(
                limit,
                claim.generation,
                claim.endpoint,
                claim.process.clone(),
                claim.build.clone(),
            )
        })
        .map_err(ClaimFailure::Registry)?;
    locator
        .publish(&PublishedLocator {
            generation: claim.generation,
            endpoint: claim.endpoint.to_owned(),
        })
        .map_err(ClaimFailure::Locator)?;
    Ok(AuthorityClaimed { recovery })
}

/// Give up this generation's registry authority.
///
/// The caller retires its endpoint and locator first: a registry that named no
/// active generation while `current.json` still named this one would be read as
/// "an authority is published that the registry does not know", which recovery
/// fails closed on. Retiring in this order leaves the two objects agreeing at
/// every point.
///
/// # Errors
/// Returns the registry failure. A generation that was never registered, or is
/// already retired, is not a failure.
pub fn release_authority<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    generation: DaemonGeneration,
) -> Result<(), RegistryFailure> {
    registry
        .update(|document| document.retire_self(generation))
        .map(|_| ())
}

#[cfg(test)]
mod tests;

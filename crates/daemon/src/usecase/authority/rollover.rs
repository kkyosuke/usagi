//! Driving the handoff across both durable objects.
//!
//! [`super::handoff`] states the protocol; this module executes it against the
//! registry, the current locator, and the local admission barrier, and puts a
//! named boundary between every durable write so a test can crash the process
//! at each one.

use std::fmt;
use std::io;

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{OperationId, ServerHello};

use crate::usecase::authority::admission::{AdmissionGate, AdmissionRefusal, LeaseClass};
use crate::usecase::authority::handoff::{
    LocatorObservation, PublishedLocator, RecoveryOutcome, RolloverOutcome, begin_handoff,
    commit_registry, complete_handoff, plan_recovery,
};
use crate::usecase::authority::registry::{
    GenerationRegistry, RegistryError, RegistryFailure, RegistryFile,
};
use crate::usecase::authority::routing::{RolloverRefusal, RoutingLedger, admit_rollover};
use crate::usecase::authority::workers::{ClientWorkers, RetireReport};
use crate::usecase::generation::{GenerationRole, ProcessIdentity, ProcessObservation};

/// The published current locator, as a port.
///
/// It is deliberately a second durable object rather than a field of the
/// registry: clients discover an endpoint without parsing the registry, and the
/// protocol is what keeps the two consistent.
pub trait CurrentLocator {
    /// Read the published locator.
    ///
    /// # Errors
    /// Returns an error only when the locator cannot be inspected at all; an
    /// absent or untrustworthy locator is an observation, not an error.
    fn read(&self) -> io::Result<LocatorObservation>;

    /// Publish `locator` as the current endpoint, replacing any predecessor.
    ///
    /// # Errors
    /// Returns an error when the locator cannot be published atomically.
    fn publish(&self, locator: &PublishedLocator) -> io::Result<()>;

    /// Remove the published locator.
    ///
    /// # Errors
    /// Returns an error when the locator cannot be removed safely.
    fn retire(&self) -> io::Result<()>;
}

/// Every boundary between two durable effects of a handoff.
///
/// The crash matrix kills the process at each of these; a fault injected here
/// is indistinguishable from the process dying there, which is exactly what
/// makes the recovery contract testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffStep {
    BeforeIntent,
    AfterIntent,
    BeforeBarrier,
    AfterBarrier,
    BeforeRegistryCommit,
    AfterRegistryCommit,
    BeforeLocatorPublish,
    AfterLocatorPublish,
    BeforeComplete,
    AfterComplete,
}

/// Why a handoff or a recovery could not finish.
#[derive(Debug)]
pub enum HandoffFailure {
    Registry(RegistryFailure),
    Admission(AdmissionRefusal),
    Locator(io::Error),
    /// A participant could not address the generation this rollover would leave
    /// draining. Refused before the first durable write (#508).
    Routing(RolloverRefusal),
}

impl From<RegistryFailure> for HandoffFailure {
    fn from(failure: RegistryFailure) -> Self {
        Self::Registry(failure)
    }
}

impl From<RegistryError> for HandoffFailure {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error.into())
    }
}

impl From<AdmissionRefusal> for HandoffFailure {
    fn from(refusal: AdmissionRefusal) -> Self {
        Self::Admission(refusal)
    }
}

impl From<io::Error> for HandoffFailure {
    fn from(error: io::Error) -> Self {
        Self::Locator(error)
    }
}

impl From<RolloverRefusal> for HandoffFailure {
    fn from(refusal: RolloverRefusal) -> Self {
        Self::Routing(refusal)
    }
}

impl fmt::Display for HandoffFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(failure) => write!(f, "{failure}"),
            Self::Admission(refusal) => write!(f, "{refusal}"),
            Self::Locator(error) => write!(f, "current locator failed: {error}"),
            Self::Routing(refusal) => write!(f, "{refusal}"),
        }
    }
}

impl std::error::Error for HandoffFailure {}

/// What a rollover was planned against, checked once before anything is written.
///
/// It exists so the shipping restart (#507) cannot start a handoff that would
/// leave a draining generation nobody can reach: the participants are named
/// here, and [`execute_gated_rollover`] refuses before the first durable write.
pub struct RolloverPlan<'a> {
    /// Every admitted client connection's routing capability.
    pub ledger: &'a RoutingLedger,
    /// The successor's own `ServerHello`, as proved by [`super::standby`].
    pub successor: &'a ServerHello,
    /// The registry revision this rollover was planned against.
    pub planned_revision: u64,
}

/// Hand authority over only when every participant can address the generation
/// this leaves draining.
///
/// This is the entry point a shipping rollover uses. The plain
/// [`execute_rollover`] performs the handoff itself and is what the fixtures
/// drive directly; it deliberately does not know about clients.
///
/// # Errors
/// Returns [`HandoffFailure::Routing`] with nothing written when a participant
/// cannot route by owner generation or the registry moved, or any failure
/// [`execute_rollover`] returns.
pub fn execute_gated_rollover<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    locator: &dyn CurrentLocator,
    gate: Option<&AdmissionGate>,
    plan: &RolloverPlan<'_>,
    operation: &OperationId,
    from: Option<DaemonGeneration>,
    to: DaemonGeneration,
) -> Result<RolloverOutcome, HandoffFailure> {
    let snapshot = registry.load()?;
    admit_rollover(
        plan.ledger,
        snapshot.document(),
        plan.planned_revision,
        plan.successor,
    )?;
    execute_rollover(registry, locator, gate, operation, from, to)
}

/// Hand authority from `from` to the verified standby `to`.
///
/// # Errors
/// Returns the first refusal or IO failure. Refusals before the registry
/// commit leave the old authority in place; a failure after it leaves the
/// handoff `committed`, which recovery rolls forward.
pub fn execute_rollover<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    locator: &dyn CurrentLocator,
    gate: Option<&AdmissionGate>,
    operation: &OperationId,
    from: Option<DaemonGeneration>,
    to: DaemonGeneration,
) -> Result<RolloverOutcome, HandoffFailure> {
    execute_rollover_with(
        registry,
        locator,
        gate,
        operation,
        from,
        to,
        &mut |_| Ok(()),
    )
}

/// As [`execute_rollover`], with a fault hook at every durable boundary.
///
/// # Errors
/// Returns the hook's error, or any failure [`execute_rollover`] returns.
pub fn execute_rollover_with<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    locator: &dyn CurrentLocator,
    gate: Option<&AdmissionGate>,
    operation: &OperationId,
    from: Option<DaemonGeneration>,
    to: DaemonGeneration,
    step: &mut dyn FnMut(HandoffStep) -> io::Result<()>,
) -> Result<RolloverOutcome, HandoffFailure> {
    step(HandoffStep::BeforeIntent)?;
    let (intent, _) = registry.update(|document| begin_handoff(document, operation, from, to))?;
    if intent == RolloverOutcome::AlreadyCompleted {
        return Ok(intent);
    }
    step(HandoffStep::AfterIntent)?;

    // The barrier precedes the commit: once it returns, this generation can no
    // longer start control work, and none is still running. An effect that has
    // already happened could not be undone by a later re-check.
    step(HandoffStep::BeforeBarrier)?;
    let barred = match gate.filter(|gate| gate.role() == GenerationRole::Active) {
        Some(gate) => {
            gate.close(LeaseClass::ActiveControl);
            gate.await_drain(LeaseClass::ActiveControl)?;
            gate.enter_draining()?;
            Some(gate)
        }
        None => None,
    };

    // Until the registry commit, the barrier is invisible outside this process,
    // so a failure here restores the old authority rather than leaving a
    // generation that is active in the registry but admits nothing.
    let committed = match commit_phase(registry, operation, step) {
        Ok(committed) => committed,
        Err(failure) => {
            // Only the gate that closed this barrier can reopen it, and it is
            // still the closer here, so reopening cannot refuse. The caller
            // must see the failure that stopped the handoff either way.
            let _ = barred.map(AdmissionGate::abort_draining);
            return Err(failure);
        }
    };
    if let Some(gate) = barred {
        gate.confirm_draining();
    }
    // Past this point the commit is durable, so a fault is a crash after W2 and
    // never a reason to reopen the barrier.
    step(HandoffStep::AfterRegistryCommit)?;

    let target = committed
        .document()
        .entry(to)
        .map(|entry| PublishedLocator {
            generation: entry.generation,
            endpoint: entry.endpoint.clone(),
        })
        .ok_or(RegistryError::UnknownGeneration)?;
    step(HandoffStep::BeforeLocatorPublish)?;
    locator.publish(&target)?;
    step(HandoffStep::AfterLocatorPublish)?;

    step(HandoffStep::BeforeComplete)?;
    let (outcome, _) = registry.update(|document| complete_handoff(document, operation))?;
    step(HandoffStep::AfterComplete)?;
    Ok(outcome)
}

/// The registry commit (W2) and the boundaries that still precede it.
///
/// A failure anywhere in here is a failure *before* the commit becomes
/// observable, which is what lets the caller reopen its barrier.
fn commit_phase<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    operation: &OperationId,
    step: &mut dyn FnMut(HandoffStep) -> io::Result<()>,
) -> Result<crate::usecase::authority::registry::RegistrySnapshot, HandoffFailure> {
    step(HandoffStep::AfterBarrier)?;
    step(HandoffStep::BeforeRegistryCommit)?;
    let (_, committed) = registry.update(|document| commit_registry(document, operation))?;
    Ok(committed)
}

/// Reconcile the registry and the locator after a restart.
///
/// The repair order is fixed: retire the locator when the outcome is fail
/// closed, publish it when rolling forward, and only then commit the registry.
/// A crash inside recovery therefore replays to the same outcome.
///
/// # Errors
/// Returns the locator or registry failure that stopped the repair.
pub fn recover<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    locator: &dyn CurrentLocator,
    observe: &mut dyn FnMut(&ProcessIdentity) -> ProcessObservation,
) -> Result<RecoveryOutcome, HandoffFailure> {
    let snapshot = registry.load()?;
    let observation = locator.read()?;
    let plan = plan_recovery(snapshot.document(), &observation, observe);
    if plan.retire_locator {
        locator.retire()?;
    }
    if let Some(target) = &plan.publish {
        locator.publish(target)?;
    }
    if let Some(document) = plan.document {
        registry.commit(&snapshot, document)?;
    }
    Ok(plan.outcome)
}

/// Retire a draining generation once its owner-terminal work is finished.
///
/// Collection stops issuing owner leases, waits for the outstanding ones to
/// reach zero, moves the gate to `retired`, unblocks and joins every retained
/// client worker, and only then records the retirement. The endpoint is the
/// caller's to reclaim afterwards.
///
/// # Errors
/// Returns [`AdmissionRefusal`] when the barrier is not satisfied, or the
/// registry failure that prevented recording the retirement.
pub fn collect_retired<F: RegistryFile>(
    registry: &GenerationRegistry<F>,
    gate: &AdmissionGate,
    workers: &ClientWorkers,
    generation: DaemonGeneration,
) -> Result<RetireReport, HandoffFailure> {
    gate.close(LeaseClass::ActiveControl);
    gate.close(LeaseClass::OwnerTerminal);
    gate.await_drain(LeaseClass::ActiveControl)?;
    gate.await_drain(LeaseClass::OwnerTerminal)?;
    gate.enter_retired()?;
    let report = workers.retire();
    registry.update(|document| document.transition(generation, GenerationRole::Retired))?;
    Ok(report)
}

#[cfg(test)]
mod tests;

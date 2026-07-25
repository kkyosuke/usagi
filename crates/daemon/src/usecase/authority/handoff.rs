//! The multi-phase authority handoff and its crash recovery.
//!
//! The registry and the current locator are two independent durable objects, so
//! no single write can move both. This module fixes one write order and states
//! what every boundary in it means after a `SIGKILL`. That is the whole point:
//! recovery never guesses, it reads the durable phase and reconciles both
//! objects against it.
//!
//! ```text
//! W1  registry CAS   handoff = { op, from, to, preparing }   nothing observable yet
//! W2  registry CAS   from→draining, to→active, current = to  commit becomes observable
//! W3  locator write  current.json names `to`                 clients follow the commit
//! W4  registry CAS   handoff cleared, op recorded done       bookkeeping only
//! ```
//!
//! | crash boundary | durable phase | recovery |
//! |---|---|---|
//! | before W1 | no handoff | old authority stands |
//! | W1..W2 | `preparing` | abort the intent; old authority stands |
//! | W2..W3 | `committed`, locator names old | roll forward: publish, then clear |
//! | W3..W4 | `committed`, locator names new | roll forward: clear only |
//! | after W4 | no handoff | new authority stands |
//!
//! A commit is never rolled back once it is observable. When the successor
//! cannot be proved alive, recovery does not resurrect the old authority
//! either: it retires every generation, removes the locator, and leaves no
//! active — an effect-zero, fail-closed state a fresh generation can start
//! from.
//!
//! The predecessor's admission barrier
//! ([`super::admission::AdmissionGate::enter_draining`]) closes between W1 and
//! W2. That is deliberately the one part of the handoff that is *not* durable:
//! until W2 nothing outside the predecessor's process has seen the role change,
//! so a failure there reopens the barrier and the old authority stands. After
//! W2 the same barrier is durable and never reopens.

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::OperationId;

use crate::usecase::authority::registry::{
    HandoffPhase, HandoffRecord, RegistryDocument, RegistryError,
};
use crate::usecase::generation::{GenerationRole, ProcessIdentity, ProcessObservation};

/// What a rollover step did. Repeating a step is never an error and never a
/// second effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloverOutcome {
    /// This call advanced the operation.
    Advanced,
    /// The operation was already in this phase; nothing changed.
    AlreadyThere,
    /// The operation already reached its terminal outcome.
    AlreadyCompleted,
}

/// The endpoint the current locator names, as observed by recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedLocator {
    pub generation: DaemonGeneration,
    pub endpoint: String,
}

/// What recovery could read from the current locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatorObservation {
    /// No locator exists.
    Absent,
    /// A locator naming a generation and endpoint.
    Published(PublishedLocator),
    /// A locator exists but is malformed or unsafe to trust.
    Unreadable,
}

/// Why recovery refused to name any authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryRefusal {
    /// The generation the commit handed authority to cannot be proved alive.
    SuccessorGone,
    /// The registry's active generation cannot be proved alive.
    ActiveGone,
    /// A locator is published although the registry holds no active
    /// generation.
    StaleCurrent,
    /// The locator cannot be read and no live authority can replace it.
    UnreadableLocator,
}

/// What recovery decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Both objects already agree.
    Consistent,
    /// An unobservable intent was dropped; the old authority stands.
    AbortedIntent(OperationId),
    /// An observable commit was completed.
    RolledForward(OperationId),
    /// No handoff was in flight, but the locator did not name the active
    /// generation's endpoint.
    RepairedCurrent,
    /// No authority survives; everything is retired and the locator removed.
    FailedClosed(RecoveryRefusal),
}

/// A repair the caller performs in exactly this order: retire the locator (when
/// asked), publish the locator (when asked), then commit the document.
///
/// Publishing before committing is what keeps the invariant across a second
/// crash: the locator may briefly lead the registry's bookkeeping, but the
/// registry's `committed` phase already names the same generation, so a repeat
/// of this plan produces the same outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub outcome: RecoveryOutcome,
    /// Remove the current locator before committing.
    pub retire_locator: bool,
    /// Publish this locator before committing.
    pub publish: Option<PublishedLocator>,
    /// Compare-and-swap this document, whose revision is already advanced.
    pub document: Option<RegistryDocument>,
}

impl RecoveryPlan {
    fn consistent() -> Self {
        Self {
            outcome: RecoveryOutcome::Consistent,
            retire_locator: false,
            publish: None,
            document: None,
        }
    }
}

/// Record the intent to hand authority from `from` to `to` (W1).
///
/// The same operation id converges: an in-flight repeat reports
/// [`RolloverOutcome::AlreadyThere`] and a finished one reports
/// [`RolloverOutcome::AlreadyCompleted`], so concurrent restarts and lost ACKs
/// never start a second handoff or a second process.
///
/// # Errors
/// Returns [`RegistryError::HandoffInProgress`] for a different in-flight
/// operation, or the refusal that makes `to` ineligible.
pub fn begin_handoff(
    document: &mut RegistryDocument,
    operation: &OperationId,
    from: Option<DaemonGeneration>,
    to: DaemonGeneration,
) -> Result<RolloverOutcome, RegistryError> {
    if document.completed_operation.as_ref() == Some(operation) {
        return Ok(RolloverOutcome::AlreadyCompleted);
    }
    if let Some(existing) = &document.handoff {
        return if &existing.operation == operation {
            Ok(RolloverOutcome::AlreadyThere)
        } else {
            Err(RegistryError::HandoffInProgress)
        };
    }
    let endpoint = eligible_successor(document, to)?;
    eligible_predecessor(document, from)?;
    document.handoff = Some(HandoffRecord {
        operation: operation.clone(),
        from,
        to,
        endpoint,
        phase: HandoffPhase::Preparing,
    });
    Ok(RolloverOutcome::Advanced)
}

/// Move roles and `current` to the successor (W2). After this commit the new
/// authority is observable and is never rolled back.
///
/// # Errors
/// Returns [`RegistryError::UnknownOperation`] when this operation is not in
/// flight, or the refusal that makes the recorded pair ineligible.
pub fn commit_registry(
    document: &mut RegistryDocument,
    operation: &OperationId,
) -> Result<RolloverOutcome, RegistryError> {
    let Some(handoff) = document.handoff.clone() else {
        return if document.completed_operation.as_ref() == Some(operation) {
            Ok(RolloverOutcome::AlreadyCompleted)
        } else {
            Err(RegistryError::UnknownOperation)
        };
    };
    if &handoff.operation != operation {
        return Err(RegistryError::UnknownOperation);
    }
    if handoff.phase == HandoffPhase::Committed {
        return Ok(RolloverOutcome::AlreadyThere);
    }
    eligible_successor(document, handoff.to)?;
    eligible_predecessor(document, handoff.from)?;
    if let Some(from) = handoff.from {
        document.transition(from, GenerationRole::Draining)?;
    }
    document.transition(handoff.to, GenerationRole::Active)?;
    document.current = Some(handoff.to);
    if let Some(record) = document.handoff.as_mut() {
        record.phase = HandoffPhase::Committed;
    }
    Ok(RolloverOutcome::Advanced)
}

/// Clear the finished handoff and record its terminal outcome (W4).
///
/// # Errors
/// Returns [`RegistryError::UnknownOperation`] or
/// [`RegistryError::WrongPhase`].
pub fn complete_handoff(
    document: &mut RegistryDocument,
    operation: &OperationId,
) -> Result<RolloverOutcome, RegistryError> {
    let Some(handoff) = document.handoff.clone() else {
        return if document.completed_operation.as_ref() == Some(operation) {
            Ok(RolloverOutcome::AlreadyCompleted)
        } else {
            Err(RegistryError::UnknownOperation)
        };
    };
    if &handoff.operation != operation {
        return Err(RegistryError::UnknownOperation);
    }
    if handoff.phase != HandoffPhase::Committed {
        return Err(RegistryError::WrongPhase);
    }
    document.handoff = None;
    document.completed_operation = Some(handoff.operation);
    Ok(RolloverOutcome::Advanced)
}

/// Drop an intent that never became observable (W1 undo).
///
/// # Errors
/// Returns [`RegistryError::UnknownOperation`] or [`RegistryError::WrongPhase`]
/// when the handoff is already committed — a committed handoff is rolled
/// forward, never aborted.
pub fn abort_handoff(
    document: &mut RegistryDocument,
    operation: &OperationId,
) -> Result<RolloverOutcome, RegistryError> {
    let Some(handoff) = document.handoff.clone() else {
        return Err(RegistryError::UnknownOperation);
    };
    if &handoff.operation != operation {
        return Err(RegistryError::UnknownOperation);
    }
    if handoff.phase != HandoffPhase::Preparing {
        return Err(RegistryError::WrongPhase);
    }
    document.handoff = None;
    document.completed_operation = Some(handoff.operation);
    Ok(RolloverOutcome::Advanced)
}

/// Reconcile the registry against the current locator after a restart.
///
/// `observe` supplies exact OS process evidence; a `VerifiedAlive` observation
/// counts only when it reports the identity the registry recorded, so PID reuse
/// never becomes proof of an owner.
#[must_use]
pub fn plan_recovery(
    document: &RegistryDocument,
    locator: &LocatorObservation,
    observe: &mut dyn FnMut(&ProcessIdentity) -> ProcessObservation,
) -> RecoveryPlan {
    let mut plan = match document.handoff.as_ref().map(|handoff| handoff.phase) {
        Some(HandoffPhase::Preparing) => plan_abort(document, locator, observe),
        Some(HandoffPhase::Committed) => plan_roll_forward(document, locator, observe),
        None => plan_steady_state(document, locator, observe),
    };
    // Every planner stages its repair at the document's stored revision; the
    // single bump here is what `GenerationRegistry::commit` compare-and-swaps
    // against.
    if let Some(staged) = plan.document.as_mut() {
        staged.revision += 1;
    }
    plan
}

fn plan_abort(
    document: &RegistryDocument,
    locator: &LocatorObservation,
    observe: &mut dyn FnMut(&ProcessIdentity) -> ProcessObservation,
) -> RecoveryPlan {
    let mut staged = document.clone();
    let operation = staged
        .handoff
        .take()
        .map(|handoff| handoff.operation)
        .expect("the caller matched a preparing handoff");
    staged.completed_operation = Some(operation.clone());
    // The intent never moved a role, so the steady-state rules decide what the
    // surviving authority is; only the abort itself is added on top.
    let mut plan = plan_steady_state(&staged, locator, observe);
    if matches!(plan.outcome, RecoveryOutcome::FailedClosed(_)) {
        return plan;
    }
    plan.outcome = RecoveryOutcome::AbortedIntent(operation);
    plan.document = Some(staged);
    plan
}

fn plan_roll_forward(
    document: &RegistryDocument,
    locator: &LocatorObservation,
    observe: &mut dyn FnMut(&ProcessIdentity) -> ProcessObservation,
) -> RecoveryPlan {
    let handoff = document
        .handoff
        .clone()
        .expect("the caller matched a committed handoff");
    let successor = document.entry(handoff.to);
    let alive = successor.is_some_and(|entry| verified_alive(&entry.process, observe));
    let Some(successor) = successor.filter(|_| alive) else {
        return fail_closed(document, RecoveryRefusal::SuccessorGone);
    };
    let target = PublishedLocator {
        generation: successor.generation,
        endpoint: successor.endpoint.clone(),
    };
    let mut staged = document.clone();
    staged.handoff = None;
    staged.completed_operation = Some(handoff.operation.clone());
    RecoveryPlan {
        outcome: RecoveryOutcome::RolledForward(handoff.operation),
        retire_locator: false,
        publish: (locator != &LocatorObservation::Published(target.clone())).then_some(target),
        document: Some(staged),
    }
}

fn plan_steady_state(
    document: &RegistryDocument,
    locator: &LocatorObservation,
    observe: &mut dyn FnMut(&ProcessIdentity) -> ProcessObservation,
) -> RecoveryPlan {
    let Some(active) = document.active() else {
        return match locator {
            LocatorObservation::Absent => RecoveryPlan::consistent(),
            LocatorObservation::Unreadable => {
                fail_closed(document, RecoveryRefusal::UnreadableLocator)
            }
            LocatorObservation::Published(_) => {
                fail_closed(document, RecoveryRefusal::StaleCurrent)
            }
        };
    };
    if !verified_alive(&active.process, observe) {
        return fail_closed(document, RecoveryRefusal::ActiveGone);
    }
    let target = PublishedLocator {
        generation: active.generation,
        endpoint: active.endpoint.clone(),
    };
    if locator == &LocatorObservation::Published(target.clone()) {
        return RecoveryPlan::consistent();
    }
    RecoveryPlan {
        outcome: RecoveryOutcome::RepairedCurrent,
        retire_locator: false,
        publish: Some(target),
        document: None,
    }
}

/// Retire every generation, drop the locator, and record any in-flight
/// operation as finished so a retry converges here instead of restarting.
fn fail_closed(document: &RegistryDocument, reason: RecoveryRefusal) -> RecoveryPlan {
    let mut staged = document.clone();
    for entry in &mut staged.generations {
        if entry.role != GenerationRole::Retired {
            entry.role = GenerationRole::Retired;
            entry.revision += 1;
        }
    }
    staged.current = None;
    if let Some(handoff) = staged.handoff.take() {
        staged.completed_operation = Some(handoff.operation);
    }
    RecoveryPlan {
        outcome: RecoveryOutcome::FailedClosed(reason),
        retire_locator: true,
        publish: None,
        document: Some(staged),
    }
}

fn verified_alive(
    process: &ProcessIdentity,
    observe: &mut dyn FnMut(&ProcessIdentity) -> ProcessObservation,
) -> bool {
    observe(process) == ProcessObservation::VerifiedAlive(process.clone())
}

/// A successor may take authority only as a verified standby.
fn eligible_successor(
    document: &RegistryDocument,
    to: DaemonGeneration,
) -> Result<String, RegistryError> {
    let entry = document.entry(to).ok_or(RegistryError::UnknownGeneration)?;
    if entry.role != GenerationRole::Standby {
        return Err(RegistryError::InvalidTransition);
    }
    if !entry.expected_build.is_known() {
        return Err(RegistryError::BuildIdentityUnknown);
    }
    if !entry.is_build_verified() {
        return Err(RegistryError::BuildMismatch);
    }
    Ok(entry.endpoint.clone())
}

/// The predecessor must be exactly the generation that holds authority now.
fn eligible_predecessor(
    document: &RegistryDocument,
    from: Option<DaemonGeneration>,
) -> Result<(), RegistryError> {
    if document.current != from {
        return Err(RegistryError::MultipleActive);
    }
    match from {
        None => Ok(()),
        Some(from) if document.role(from) == Some(GenerationRole::Active) => Ok(()),
        Some(_) => Err(RegistryError::InvalidTransition),
    }
}

#[cfg(test)]
mod tests;

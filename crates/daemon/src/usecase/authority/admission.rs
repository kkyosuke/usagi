//! Per-request admission and the RAII lease that fences a handoff.
//!
//! Two mistakes this module exists to prevent:
//!
//! 1. **Treating an established connection as authority.** A connection opened
//!    while this generation was `active` keeps its frames flowing after the
//!    role changes, so authority is re-decided for *every* request from the
//!    live role, revision, and the resource's owner — never from the fact that
//!    the peer got in.
//! 2. **Fencing only the accept loop.** Stopping new connections says nothing
//!    about a spawn that is already between its durable reservation and its
//!    external effect. Active-only work therefore takes an
//!    [`AdmissionLease`] *before* it reserves anything and holds it until the
//!    durable commit is done; `active → draining` closes lease issuance and
//!    waits for the outstanding leases to reach zero *before* the registry and
//!    locator handoff is committed. Re-checking the role after an effect cannot
//!    un-spawn a process, so the barrier comes first.
//!
//! Owner-terminal work lives on a second lease class: a draining generation
//! must keep serving the terminals it already owns while every control path is
//! already closed, and its collection may only start once that class has itself
//! stopped issuing and drained.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use usagi_core::domain::id::DaemonGeneration;

use crate::usecase::generation::GenerationRole;

/// The two independently fenced kinds of admitted work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseClass {
    /// Control operations, new spawns, and active-only background producers
    /// (supervisor tick, decision worker, PR refresh).
    ActiveControl,
    /// IO on a terminal this generation already owns. It outlives the control
    /// barrier so a draining owner can keep serving its PTYs.
    OwnerTerminal,
}

/// What kind of work a request performs. Classification is the caller's, so a
/// new request type cannot silently default into "control".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// The active daemon's own handoff trigger. It must not take the
    /// `ActiveControl` lease whose drain it is about to wait for.
    Rollover,
    /// Session/agent lifecycle and any other control-plane mutation.
    Control,
    /// Creating a new daemon-owned runtime.
    Spawn,
    /// attach / input / resize / resync / exit / kill on an existing terminal.
    TerminalIo,
    /// A read that mutates nothing.
    Read,
    /// Listing runtimes.
    Inventory,
}

/// Who owns the resource a request names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOwner {
    /// This generation owns it.
    SelfGeneration,
    /// Another generation owns it.
    OtherGeneration,
    /// The request names no owned resource.
    Unscoped,
}

/// Why a request or lease was refused. Every variant is effect zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionRefusal {
    /// The generation does not hold control authority.
    NotActive,
    /// Lease issuance for this class is closed — a handoff is in progress or
    /// finished.
    Closed,
    /// The resource belongs to another generation.
    NotOwner,
    /// A retired generation admits nothing at all.
    Retired,
    /// The caller's authority moved on since its lease was issued.
    StaleRevision,
    /// The class is still issuing leases, so a drain barrier cannot be waited
    /// on yet.
    StillOpen,
}

impl fmt::Display for AdmissionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotActive => "generation does not hold control authority",
            Self::Closed => "generation stopped admitting this work",
            Self::NotOwner => "resource belongs to another generation",
            Self::Retired => "generation is retired",
            Self::StaleRevision => "generation authority moved on",
            Self::StillOpen => "generation is still admitting this work",
        })
    }
}

impl std::error::Error for AdmissionRefusal {}

/// Decide whether a request is admissible and which lease it needs.
///
/// `Ok(None)` means the request is admissible without a lease: it produces no
/// effect a handoff could have to wait for.
///
/// # Errors
/// Returns the refusal that fails the request closed.
pub fn classify(
    role: GenerationRole,
    request: RequestClass,
    owner: ResourceOwner,
) -> Result<Option<LeaseClass>, AdmissionRefusal> {
    if owner == ResourceOwner::OtherGeneration {
        return Err(AdmissionRefusal::NotOwner);
    }
    match role {
        GenerationRole::Retired => Err(AdmissionRefusal::Retired),
        GenerationRole::Standby => match request {
            // A standby must reach readiness without touching anything. Reads
            // are how it proves it can serve at all.
            RequestClass::Read | RequestClass::Inventory => Ok(None),
            _ => Err(AdmissionRefusal::NotActive),
        },
        GenerationRole::Draining => match request {
            RequestClass::Read | RequestClass::Inventory => Ok(None),
            RequestClass::TerminalIo if owner == ResourceOwner::SelfGeneration => {
                Ok(Some(LeaseClass::OwnerTerminal))
            }
            // An unscoped terminal request cannot prove ownership, so it is
            // refused rather than resolved against the whole registry.
            RequestClass::TerminalIo => Err(AdmissionRefusal::NotOwner),
            RequestClass::Rollover | RequestClass::Control | RequestClass::Spawn => {
                Err(AdmissionRefusal::NotActive)
            }
        },
        GenerationRole::Active => match request {
            RequestClass::Rollover | RequestClass::Read | RequestClass::Inventory => Ok(None),
            RequestClass::TerminalIo if owner == ResourceOwner::SelfGeneration => {
                Ok(Some(LeaseClass::OwnerTerminal))
            }
            RequestClass::TerminalIo => Err(AdmissionRefusal::NotOwner),
            RequestClass::Control | RequestClass::Spawn => Ok(Some(LeaseClass::ActiveControl)),
        },
    }
}

#[derive(Debug)]
struct GateState {
    role: GenerationRole,
    revision: u64,
    active_open: bool,
    owner_open: bool,
    active_leases: usize,
    owner_leases: usize,
    /// Set when this process closed its own control barrier for a handoff that
    /// has not committed durably yet. Only such a barrier may be reopened.
    barred: bool,
}

impl GateState {
    fn open(&self, class: LeaseClass) -> bool {
        match class {
            LeaseClass::ActiveControl => self.active_open,
            LeaseClass::OwnerTerminal => self.owner_open,
        }
    }

    fn count(&self, class: LeaseClass) -> usize {
        match class {
            LeaseClass::ActiveControl => self.active_leases,
            LeaseClass::OwnerTerminal => self.owner_leases,
        }
    }

    fn count_mut(&mut self, class: LeaseClass) -> &mut usize {
        match class {
            LeaseClass::ActiveControl => &mut self.active_leases,
            LeaseClass::OwnerTerminal => &mut self.owner_leases,
        }
    }
}

#[derive(Debug)]
struct Gate {
    generation: DaemonGeneration,
    state: Mutex<GateState>,
    drained: Condvar,
}

/// The process-local half of the cross-process authority: it decides, per
/// request, whether this generation may act, and it is the barrier a handoff
/// waits on.
#[derive(Debug, Clone)]
pub struct AdmissionGate(Arc<Gate>);

impl AdmissionGate {
    /// Open a gate for `generation` in `role` at revision 1.
    #[must_use]
    pub fn new(generation: DaemonGeneration, role: GenerationRole) -> Self {
        Self(Arc::new(Gate {
            generation,
            state: Mutex::new(GateState {
                role,
                revision: 1,
                active_open: role == GenerationRole::Active,
                owner_open: matches!(role, GenerationRole::Active | GenerationRole::Draining),
                active_leases: 0,
                owner_leases: 0,
                barred: false,
            }),
            drained: Condvar::new(),
        }))
    }

    /// The generation this gate speaks for.
    #[must_use]
    pub fn generation(&self) -> DaemonGeneration {
        self.0.generation
    }

    /// The live role.
    #[must_use]
    pub fn role(&self) -> GenerationRole {
        self.state().role
    }

    /// The live authority revision. It advances on every role change, so a
    /// lease taken under the old role can be told apart from a new one.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.state().revision
    }

    /// Whether this class still issues leases. Background producers poll this
    /// to stop before the barrier waits on them.
    #[must_use]
    pub fn is_open(&self, class: LeaseClass) -> bool {
        self.state().open(class)
    }

    /// How many leases of `class` are outstanding.
    #[must_use]
    pub fn outstanding(&self, class: LeaseClass) -> usize {
        self.state().count(class)
    }

    /// Take a lease. The caller must hold it across its durable reservation,
    /// its external effect, and its durable commit.
    ///
    /// # Errors
    /// Returns [`AdmissionRefusal::Closed`] once the class stopped issuing, or
    /// [`AdmissionRefusal::NotActive`] / [`AdmissionRefusal::Retired`] when the
    /// role does not permit this class at all.
    pub fn acquire(&self, class: LeaseClass) -> Result<AdmissionLease, AdmissionRefusal> {
        let mut state = self.state();
        match (class, state.role) {
            (_, GenerationRole::Retired) => return Err(AdmissionRefusal::Retired),
            (LeaseClass::ActiveControl, GenerationRole::Active)
            | (LeaseClass::OwnerTerminal, GenerationRole::Active | GenerationRole::Draining) => {}
            _ => return Err(AdmissionRefusal::NotActive),
        }
        if !state.open(class) {
            return Err(AdmissionRefusal::Closed);
        }
        *state.count_mut(class) += 1;
        Ok(AdmissionLease {
            gate: Arc::clone(&self.0),
            class,
            generation: self.0.generation,
            revision: state.revision,
        })
    }

    /// Classify `request` against the live role and take the lease it needs.
    ///
    /// This is the per-request check: it reads the role at the moment the
    /// request is dispatched, so a connection established under a previous role
    /// gains nothing from having been admitted earlier.
    ///
    /// # Errors
    /// Returns the classification refusal, or the acquisition refusal.
    pub fn admit(
        &self,
        request: RequestClass,
        owner: ResourceOwner,
    ) -> Result<Option<AdmissionLease>, AdmissionRefusal> {
        match classify(self.role(), request, owner)? {
            None => Ok(None),
            Some(class) => self.acquire(class).map(Some),
        }
    }

    /// Stop issuing leases of `class`. Idempotent.
    pub fn close(&self, class: LeaseClass) {
        let mut state = self.state();
        match class {
            LeaseClass::ActiveControl => state.active_open = false,
            LeaseClass::OwnerTerminal => state.owner_open = false,
        }
        drop(state);
        self.0.drained.notify_all();
    }

    /// Block until every outstanding lease of `class` is released.
    ///
    /// # Errors
    /// Returns [`AdmissionRefusal::StillOpen`] when the class was not closed
    /// first — waiting on an open class could never terminate.
    pub fn await_drain(&self, class: LeaseClass) -> Result<(), AdmissionRefusal> {
        let state = self.state();
        if state.open(class) {
            return Err(AdmissionRefusal::StillOpen);
        }
        // `wait_while` keeps the barrier on one code path whether or not the
        // caller actually has to block, so the drained case and the parked case
        // do not diverge into separate branches.
        let _drained = self
            .0
            .drained
            .wait_while(state, |state| state.count(class) > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(())
    }

    /// Adopt the authority the registry granted this standby.
    ///
    /// The registry commit is what makes a generation active; this only brings
    /// the process-local gate in line with it, so a generation cannot admit
    /// control work before the durable commit that named it.
    ///
    /// # Errors
    /// Returns [`AdmissionRefusal::NotActive`] when the gate is not a standby —
    /// a draining or retired generation never returns to `active`.
    pub fn activate(&self) -> Result<u64, AdmissionRefusal> {
        let mut state = self.state();
        if state.role != GenerationRole::Standby {
            return Err(AdmissionRefusal::NotActive);
        }
        state.role = GenerationRole::Active;
        state.active_open = true;
        state.owner_open = true;
        state.revision += 1;
        Ok(state.revision)
    }

    /// Move `active → draining` once control work is closed and drained.
    ///
    /// Call this *before* committing the registry and locator handoff: after it
    /// returns, no new control lease can be issued and none is outstanding, so
    /// the handoff cannot leave a late spawn behind. Owner-terminal leases keep
    /// being issued.
    ///
    /// # Errors
    /// Returns [`AdmissionRefusal::NotActive`] when the gate is not active,
    /// [`AdmissionRefusal::StillOpen`] when control work was not closed, or
    /// [`AdmissionRefusal::Closed`] when leases are still outstanding.
    pub fn enter_draining(&self) -> Result<u64, AdmissionRefusal> {
        let mut state = self.state();
        if state.role != GenerationRole::Active {
            return Err(AdmissionRefusal::NotActive);
        }
        if state.active_open {
            return Err(AdmissionRefusal::StillOpen);
        }
        if state.active_leases > 0 {
            return Err(AdmissionRefusal::Closed);
        }
        state.role = GenerationRole::Draining;
        state.barred = true;
        state.revision += 1;
        Ok(state.revision)
    }

    /// Reopen a barrier this process closed for a handoff that never committed.
    ///
    /// The barrier is process local: until the registry commit (W2) makes the
    /// successor observable, nothing outside this process saw the role change,
    /// so a failed handoff restores the old authority instead of leaving a
    /// generation that admits nothing. Only the barrier
    /// [`enter_draining`](Self::enter_draining) set can be reopened — a
    /// generation that is durably draining never returns to `active`.
    ///
    /// # Errors
    /// Returns [`AdmissionRefusal::NotActive`] when this gate did not close its
    /// own pre-commit barrier.
    pub fn abort_draining(&self) -> Result<u64, AdmissionRefusal> {
        let mut state = self.state();
        if !state.barred || state.role != GenerationRole::Draining {
            return Err(AdmissionRefusal::NotActive);
        }
        state.role = GenerationRole::Active;
        state.active_open = true;
        state.barred = false;
        state.revision += 1;
        Ok(state.revision)
    }

    /// Confirm that the barrier this process closed is now durable, so it can
    /// no longer be reopened.
    pub fn confirm_draining(&self) {
        self.state().barred = false;
    }

    /// Move to `retired` once owner-terminal work is closed and drained. The
    /// caller reclaims the endpoint and the process only after this returns.
    ///
    /// # Errors
    /// Returns [`AdmissionRefusal::StillOpen`] when either class still issues
    /// leases, or [`AdmissionRefusal::Closed`] when any lease is outstanding.
    pub fn enter_retired(&self) -> Result<u64, AdmissionRefusal> {
        let mut state = self.state();
        if state.active_open || state.owner_open {
            return Err(AdmissionRefusal::StillOpen);
        }
        if state.active_leases > 0 || state.owner_leases > 0 {
            return Err(AdmissionRefusal::Closed);
        }
        state.role = GenerationRole::Retired;
        state.revision += 1;
        Ok(state.revision)
    }

    fn state(&self) -> MutexGuard<'_, GateState> {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Proof that this generation may keep producing an effect. Dropping it is what
/// lets a handoff barrier proceed, so it is held — not checked and discarded —
/// across reservation, effect, and commit.
#[derive(Debug)]
pub struct AdmissionLease {
    gate: Arc<Gate>,
    class: LeaseClass,
    generation: DaemonGeneration,
    revision: u64,
}

impl AdmissionLease {
    /// The class this lease was issued for.
    #[must_use]
    pub fn class(&self) -> LeaseClass {
        self.class
    }

    /// The generation that issued it.
    #[must_use]
    pub fn generation(&self) -> DaemonGeneration {
        self.generation
    }

    /// The authority revision it was issued under.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Re-check that the authority has not moved since issuance.
    ///
    /// This is a diagnostic for the durable commit path, never a substitute for
    /// holding the lease: an effect that already happened cannot be undone by
    /// noticing afterwards that the role changed.
    ///
    /// # Errors
    /// Returns [`AdmissionRefusal::StaleRevision`].
    pub fn revalidate(&self) -> Result<(), AdmissionRefusal> {
        let state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.revision == self.revision {
            Ok(())
        } else {
            Err(AdmissionRefusal::StaleRevision)
        }
    }
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = state.count_mut(self.class);
        *count = count.saturating_sub(1);
        drop(state);
        self.gate.drained.notify_all();
    }
}

#[cfg(test)]
mod tests;

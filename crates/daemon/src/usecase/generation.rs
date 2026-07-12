//! Durable daemon-generation ownership and crash reconciliation.
//!
//! A generation registry is the authority for routing: control work is accepted
//! only by `current`, while a terminal is routed only to the generation stored
//! in its [`TerminalRef`].  In particular, an endpoint is never copied into a
//! terminal reference, and a PID is never enough evidence to signal a process.

#![allow(clippy::missing_errors_doc)] // Every error is a documented fencing outcome in GenerationError.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{DaemonGeneration, SessionId, TerminalRef, WorktreeId};

/// The maximum number of simultaneously retained daemon generations.
pub const DEFAULT_GENERATION_LIMIT: usize = 2;

/// A daemon generation's authority role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationRole {
    Standby,
    Active,
    Draining,
    Retired,
}

/// A trusted, generation-local IPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub generation: DaemonGeneration,
    pub endpoint: String,
    pub role: GenerationRole,
}

/// The current locator and all retained generation records.  Persist this as
/// one compare-and-swap value; publishing the locator separately would allow
/// two active control writers after a crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationSnapshot {
    pub current: Option<DaemonGeneration>,
    pub records: Vec<GenerationRecord>,
    pub terminals: Vec<TerminalOwnership>,
}

/// Process identity recorded after spawn.  `start_identity` is platform
/// supplied (for example a process start time); it must not be inferred from a
/// PID. `process_group` makes group signalling fenceable as well.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_identity: String,
    pub process_group: u32,
}

/// What can safely be said about a terminal after its owning daemon died.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Available,
    OrphanRunning,
    IdentityUnknown,
    Lost,
    Terminated,
}

/// Durable terminal ownership.  This survives a daemon crash even when the
/// PTY master and output journal do not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOwnership {
    pub terminal: TerminalRef,
    pub process: Option<ProcessIdentity>,
    pub state: TerminalState,
}

/// Result of an OS process observation made during reconcile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessObservation {
    /// The exact recorded process identity is still alive.
    VerifiedAlive(ProcessIdentity),
    /// The recorded process is known gone.
    Gone,
    /// PID reuse, an unreadable registry, or incomplete spawn evidence.
    Unknown,
}

/// A registry refusal is a safety result, not a reason to fall back to a name,
/// PID, or a newly spawned terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationError {
    UnknownGeneration,
    DuplicateGeneration,
    NotStandby,
    NotActive,
    GenerationLimit,
    Busy,
    TerminalOwnedElsewhere,
    TerminalUnavailable,
    ReplacementBlocked,
}

/// Pure coordinator state.  The caller persists [`GenerationSnapshot`] with a
/// CAS around each mutating method; this module deliberately has no filesystem
/// or process side effects.
#[derive(Debug, Clone)]
pub struct GenerationCoordinator {
    limit: usize,
    current: Option<DaemonGeneration>,
    records: BTreeMap<String, GenerationRecord>,
    terminals: BTreeMap<String, TerminalOwnership>,
}

impl GenerationCoordinator {
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            current: None,
            records: BTreeMap::new(),
            terminals: BTreeMap::new(),
        }
    }

    /// Restores only a self-consistent snapshot. Corruption fails closed rather
    /// than forgetting terminal ownership.
    pub fn restore(snapshot: GenerationSnapshot, limit: usize) -> Result<Self, GenerationError> {
        let mut coordinator = Self::new(limit);
        for record in snapshot.records {
            coordinator.register_record(record)?;
        }
        coordinator.current = snapshot.current;
        if let Some(current) = coordinator.current
            && coordinator.role(current) != Some(GenerationRole::Active)
        {
            return Err(GenerationError::NotActive);
        }
        for terminal in snapshot.terminals {
            let key = terminal_key(&terminal.terminal);
            if coordinator.terminals.insert(key, terminal).is_some() {
                return Err(GenerationError::DuplicateGeneration);
            }
        }
        Ok(coordinator)
    }

    #[must_use]
    pub fn snapshot(&self) -> GenerationSnapshot {
        GenerationSnapshot {
            current: self.current,
            records: self.records.values().cloned().collect(),
            terminals: self.terminals.values().cloned().collect(),
        }
    }

    /// Adds a listener that is ready to take part in a handoff but cannot yet
    /// mutate session/control state.
    pub fn register_standby(
        &mut self,
        generation: DaemonGeneration,
        endpoint: String,
    ) -> Result<(), GenerationError> {
        self.register_record(GenerationRecord {
            generation,
            endpoint,
            role: GenerationRole::Standby,
        })
    }

    fn register_record(&mut self, record: GenerationRecord) -> Result<(), GenerationError> {
        if self.records.contains_key(&record.generation.as_str()) {
            return Err(GenerationError::DuplicateGeneration);
        }
        if record.role != GenerationRole::Retired && self.retained_generations() >= self.limit {
            return Err(GenerationError::GenerationLimit);
        }
        self.records.insert(record.generation.as_str(), record);
        Ok(())
    }

    /// Makes the first registered daemon active.
    pub fn activate_initial(
        &mut self,
        generation: DaemonGeneration,
    ) -> Result<(), GenerationError> {
        if self.current.is_some() {
            return Err(GenerationError::NotActive);
        }
        self.set_role(generation, GenerationRole::Standby, GenerationRole::Active)?;
        self.current = Some(generation);
        Ok(())
    }

    /// Atomically performs the control handoff after `next` has become ready.
    /// A running non-terminal external effect makes rollover unsafe; queued
    /// work can be re-owned only after its old worker has stopped.
    pub fn rollover(
        &mut self,
        active: DaemonGeneration,
        next: DaemonGeneration,
        running_nonterminal_external_io: bool,
    ) -> Result<(), GenerationError> {
        self.require_active(active)?;
        if running_nonterminal_external_io {
            return Err(GenerationError::Busy);
        }
        if self.role(next) != Some(GenerationRole::Standby) {
            return Err(GenerationError::NotStandby);
        }
        self.set_role(active, GenerationRole::Active, GenerationRole::Draining)?;
        self.set_role(next, GenerationRole::Standby, GenerationRole::Active)?;
        self.current = Some(next);
        Ok(())
    }

    /// Rechecks control authority immediately before an effect and again before
    /// its state commit. Callers use this on both sides of external IO.
    pub fn require_active(&self, generation: DaemonGeneration) -> Result<(), GenerationError> {
        (self.current == Some(generation) && self.role(generation) == Some(GenerationRole::Active))
            .then_some(())
            .ok_or(GenerationError::NotActive)
    }

    /// Resolves a terminal only through its owner generation's trusted record.
    /// Draining generations intentionally remain routable for terminal IO.
    pub fn terminal_endpoint(&self, terminal: &TerminalRef) -> Result<&str, GenerationError> {
        let ownership = self
            .terminals
            .get(&terminal_key(terminal))
            .filter(|known| known.terminal.fences(terminal))
            .ok_or(GenerationError::TerminalOwnedElsewhere)?;
        if ownership.state != TerminalState::Available {
            return Err(GenerationError::TerminalUnavailable);
        }
        self.records
            .get(&terminal.daemon_generation.as_str())
            .filter(|record| {
                matches!(
                    record.role,
                    GenerationRole::Active | GenerationRole::Draining
                )
            })
            .map(|record| record.endpoint.as_str())
            .ok_or(GenerationError::UnknownGeneration)
    }

    /// Stores terminal ownership before spawn. A missing identity after crash is
    /// deliberately `identity_unknown`, never evidence that no child exists.
    pub fn reserve_terminal(&mut self, terminal: TerminalRef) -> Result<(), GenerationError> {
        self.require_active(terminal.daemon_generation)?;
        let key = terminal_key(&terminal);
        if self.terminals.contains_key(&key) {
            return Err(GenerationError::TerminalOwnedElsewhere);
        }
        self.terminals.insert(
            key,
            TerminalOwnership {
                terminal,
                process: None,
                state: TerminalState::IdentityUnknown,
            },
        );
        Ok(())
    }

    pub fn record_spawn(
        &mut self,
        terminal: &TerminalRef,
        process: ProcessIdentity,
    ) -> Result<(), GenerationError> {
        let ownership = self.ownership_mut(terminal)?;
        ownership.process = Some(process);
        ownership.state = TerminalState::Available;
        Ok(())
    }

    /// Marks every terminal of a crashed generation as non-attachable. Exact
    /// identity evidence produces `orphan_running`; all other evidence is
    /// `identity_unknown` and must never be signalled.
    pub fn crash_generation<F>(&mut self, generation: DaemonGeneration, mut observe: F)
    where
        F: FnMut(&ProcessIdentity) -> ProcessObservation,
    {
        for ownership in self
            .terminals
            .values_mut()
            .filter(|entry| entry.terminal.daemon_generation == generation)
        {
            ownership.state = match ownership.process.as_ref().map(&mut observe) {
                Some(ProcessObservation::VerifiedAlive(actual))
                    if ownership.process.as_ref() == Some(&actual) =>
                {
                    TerminalState::OrphanRunning
                }
                Some(ProcessObservation::Gone) => TerminalState::Lost,
                _ => TerminalState::IdentityUnknown,
            };
        }
        if let Some(record) = self.records.get_mut(&generation.as_str()) {
            record.role = GenerationRole::Retired;
        }
        if self.current == Some(generation) {
            self.current = None;
        }
    }

    /// Replacement Agent spawn is blocked while an orphan from the same session
    /// and worktree remains unresolved.
    #[must_use]
    pub fn replacement_allowed(&self, session: SessionId, worktree: WorktreeId) -> bool {
        !self.terminals.values().any(|ownership| {
            ownership.terminal.session_id == Some(session)
                && ownership.terminal.worktree_id == worktree
                && matches!(
                    ownership.state,
                    TerminalState::OrphanRunning | TerminalState::IdentityUnknown
                )
        })
    }

    /// Reconcile a terminal only after verified process disappearance, a
    /// completed terminate ACK, or an explicit human acknowledgement.
    pub fn resolve_orphan(
        &mut self,
        terminal: &TerminalRef,
        observation: ProcessObservation,
        acknowledged: bool,
    ) -> Result<(), GenerationError> {
        let ownership = self.ownership_mut(terminal)?;
        match observation {
            ProcessObservation::Gone => ownership.state = TerminalState::Lost,
            ProcessObservation::VerifiedAlive(actual)
                if ownership.process.as_ref() == Some(&actual) && acknowledged =>
            {
                ownership.state = TerminalState::Terminated;
            }
            _ if acknowledged => ownership.state = TerminalState::Terminated,
            _ => return Err(GenerationError::TerminalUnavailable),
        }
        Ok(())
    }

    /// Retires a draining daemon only after every owned terminal is resolved.
    pub fn collect_draining(
        &mut self,
        generation: DaemonGeneration,
    ) -> Result<bool, GenerationError> {
        if self.role(generation) != Some(GenerationRole::Draining) {
            return Err(GenerationError::NotActive);
        }
        let live = self.terminals.values().any(|terminal| {
            terminal.terminal.daemon_generation == generation
                && matches!(
                    terminal.state,
                    TerminalState::Available
                        | TerminalState::OrphanRunning
                        | TerminalState::IdentityUnknown
                )
        });
        if live {
            return Ok(false);
        }
        let record = self
            .records
            .get_mut(&generation.as_str())
            .ok_or(GenerationError::UnknownGeneration)?;
        record.role = GenerationRole::Retired;
        Ok(true)
    }

    fn ownership_mut(
        &mut self,
        terminal: &TerminalRef,
    ) -> Result<&mut TerminalOwnership, GenerationError> {
        self.terminals
            .get_mut(&terminal_key(terminal))
            .filter(|known| known.terminal.fences(terminal))
            .ok_or(GenerationError::TerminalOwnedElsewhere)
    }
    fn role(&self, generation: DaemonGeneration) -> Option<GenerationRole> {
        self.records
            .get(&generation.as_str())
            .map(|record| record.role)
    }
    fn retained_generations(&self) -> usize {
        self.records
            .values()
            .filter(|record| record.role != GenerationRole::Retired)
            .count()
    }
    fn set_role(
        &mut self,
        generation: DaemonGeneration,
        expected: GenerationRole,
        role: GenerationRole,
    ) -> Result<(), GenerationError> {
        let record = self
            .records
            .get_mut(&generation.as_str())
            .ok_or(GenerationError::UnknownGeneration)?;
        if record.role != expected {
            return Err(GenerationError::NotActive);
        }
        record.role = role;
        Ok(())
    }
}

fn terminal_key(terminal: &TerminalRef) -> String {
    terminal.terminal_id.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::id::{SessionId, TerminalId, WorkspaceId};

    fn generation() -> DaemonGeneration {
        DaemonGeneration::new()
    }
    fn coordinator() -> (GenerationCoordinator, DaemonGeneration) {
        let mut registry = GenerationCoordinator::new(DEFAULT_GENERATION_LIMIT);
        let active = generation();
        registry
            .register_standby(active, "active.sock".into())
            .unwrap();
        registry.activate_initial(active).unwrap();
        (registry, active)
    }
    fn terminal(owner: DaemonGeneration) -> TerminalRef {
        TerminalRef {
            daemon_generation: owner,
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }
    fn identity() -> ProcessIdentity {
        ProcessIdentity {
            pid: 7,
            start_identity: "start-7".into(),
            process_group: 7,
        }
    }

    #[test]
    fn rollover_routes_live_terminal_to_draining_owner_and_fences_old_control() {
        let (mut registry, old) = coordinator();
        let pane = terminal(old);
        registry.reserve_terminal(pane.clone()).unwrap();
        registry.record_spawn(&pane, identity()).unwrap();
        let next = generation();
        registry.register_standby(next, "next.sock".into()).unwrap();
        registry.rollover(old, next, false).unwrap();
        assert_eq!(registry.terminal_endpoint(&pane), Ok("active.sock"));
        assert_eq!(
            registry.require_active(old),
            Err(GenerationError::NotActive)
        );
        assert_eq!(registry.require_active(next), Ok(()));
    }
    #[test]
    fn rollover_is_busy_for_running_external_io_and_never_allows_a_third_generation() {
        let (mut registry, old) = coordinator();
        let next = generation();
        registry.register_standby(next, "next.sock".into()).unwrap();
        assert_eq!(
            registry.rollover(old, next, true),
            Err(GenerationError::Busy)
        );
        registry.rollover(old, next, false).unwrap();
        assert_eq!(
            registry.register_standby(generation(), "third.sock".into()),
            Err(GenerationError::GenerationLimit)
        );
        assert!(registry.collect_draining(old).unwrap());
        assert!(
            registry
                .register_standby(generation(), "third.sock".into())
                .is_ok()
        );
    }
    #[test]
    fn pid_reuse_becomes_identity_unknown_and_blocks_attach_replacement_and_signal_assumptions() {
        let (mut registry, owner) = coordinator();
        let pane = terminal(owner);
        let session = pane.session_id.unwrap();
        let worktree = pane.worktree_id;
        registry.reserve_terminal(pane.clone()).unwrap();
        registry.record_spawn(&pane, identity()).unwrap();
        registry.crash_generation(owner, |_| {
            ProcessObservation::VerifiedAlive(ProcessIdentity {
                pid: 7,
                start_identity: "reused".into(),
                process_group: 7,
            })
        });
        assert_eq!(
            registry.terminal_endpoint(&pane),
            Err(GenerationError::TerminalUnavailable)
        );
        assert!(!registry.replacement_allowed(session, worktree));
        assert_eq!(
            registry.resolve_orphan(&pane, ProcessObservation::Unknown, false),
            Err(GenerationError::TerminalUnavailable)
        );
    }
    #[test]
    fn crash_with_verified_child_is_orphan_until_explicitly_acknowledged() {
        let (mut registry, owner) = coordinator();
        let pane = terminal(owner);
        registry.reserve_terminal(pane.clone()).unwrap();
        let process = identity();
        registry.record_spawn(&pane, process.clone()).unwrap();
        registry.crash_generation(owner, |_| {
            ProcessObservation::VerifiedAlive(process.clone())
        });
        assert_eq!(
            registry.snapshot().terminals[0].state,
            TerminalState::OrphanRunning
        );
        registry
            .resolve_orphan(&pane, ProcessObservation::VerifiedAlive(process), true)
            .unwrap();
        assert_eq!(
            registry.snapshot().terminals[0].state,
            TerminalState::Terminated
        );
    }
    #[test]
    fn incomplete_spawn_record_is_never_deleted_or_replaced_after_crash() {
        let (mut registry, owner) = coordinator();
        let pane = terminal(owner);
        let session = pane.session_id.unwrap();
        let worktree = pane.worktree_id;
        registry.reserve_terminal(pane.clone()).unwrap();
        registry.crash_generation(owner, |_| ProcessObservation::Gone);
        assert_eq!(
            registry.snapshot().terminals[0].state,
            TerminalState::IdentityUnknown
        );
        assert!(!registry.replacement_allowed(session, worktree));
    }
}

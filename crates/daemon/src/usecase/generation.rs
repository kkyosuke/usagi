//! Persisted per-runtime generation and terminal ownership fences.
//!
//! The process-local snapshot retains terminal owners: local work is accepted
//! only by `current`, while a terminal is routed only to the generation stored
//! in its [`TerminalRef`].  In particular, an endpoint is never copied into a
//! terminal reference, and a PID is never enough evidence to signal a process.
//! Cross-process activation, rollover and collection belong to [`super::authority`].

#![allow(clippy::missing_errors_doc)] // Every error is a documented fencing outcome in GenerationError.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{DaemonGeneration, TerminalRef};
use usagi_core::infrastructure::ipc::BuildIdentity;

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
    /// Legacy artifact metadata retained for snapshot compatibility. Shipping
    /// handoff verification is owned by the durable authority registry.
    #[serde(default)]
    pub expected_build: BuildIdentity,
    /// Legacy verification metadata; preserved when existing snapshots are read.
    #[serde(default)]
    pub build_verified: bool,
}

/// Generation fences persisted with one runtime's records. The cross-process
/// current locator and handoff transaction live in [`super::authority`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
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
    NotActive,
    GenerationLimit,
    TerminalOwnedElsewhere,
    TerminalUnavailable,
}

/// Process-local ownership state persisted with the runtime snapshot. This
/// module deliberately has no filesystem or process side effects.
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
        let active_records = coordinator
            .records
            .values()
            .filter(|record| record.role == GenerationRole::Active)
            .count();
        if active_records != usize::from(coordinator.current.is_some()) {
            return Err(GenerationError::NotActive);
        }
        for terminal in snapshot.terminals {
            let Some(owner) = coordinator
                .records
                .get(&terminal.terminal.daemon_generation.as_str())
            else {
                return Err(GenerationError::UnknownGeneration);
            };
            if terminal.state == TerminalState::Available
                && !matches!(
                    owner.role,
                    GenerationRole::Active | GenerationRole::Draining
                )
            {
                return Err(GenerationError::TerminalUnavailable);
            }
            if terminal.process.is_none()
                && matches!(
                    terminal.state,
                    TerminalState::Available | TerminalState::OrphanRunning
                )
            {
                return Err(GenerationError::TerminalUnavailable);
            }
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

    /// Returns the only generation allowed to admit control work.
    #[must_use]
    pub fn current(&self) -> Option<DaemonGeneration> {
        self.current
    }

    /// Verifies exact terminal ownership without exposing or guessing an
    /// endpoint. Agent runtimes use this before every local PTY effect.
    pub fn require_terminal(&self, terminal: &TerminalRef) -> Result<(), GenerationError> {
        self.terminal_endpoint(terminal).map(|_| ())
    }

    /// Returns whether the exact terminal identity belongs to this authority,
    /// including retained exited/lost records. Routing uses this to return a
    /// fenced stale result instead of falling through to another owner.
    #[must_use]
    pub fn owns_terminal(&self, terminal: &TerminalRef) -> bool {
        self.terminals
            .get(&terminal_key(terminal))
            .is_some_and(|known| known.terminal.fences(terminal))
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
            expected_build: BuildIdentity::default(),
            build_verified: false,
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
                record.role == GenerationRole::Active || record.role == GenerationRole::Draining
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
            ProcessObservation::Unknown if !acknowledged => {
                ownership.state = TerminalState::IdentityUnknown;
                return Err(GenerationError::TerminalUnavailable);
            }
            ProcessObservation::VerifiedAlive(actual)
                if ownership.process.as_ref() == Some(&actual) && !acknowledged =>
            {
                ownership.state = TerminalState::OrphanRunning;
            }
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

    /// Drops historical process evidence after the runtime record has done the
    /// same. Live or unresolved ownership may not discard its only exact
    /// identity; resolved terminals can do so while preserving their terminal
    /// tombstone and generation fence.
    pub fn forget_resolved_process(
        &mut self,
        terminal: &TerminalRef,
    ) -> Result<(), GenerationError> {
        let ownership = self.ownership_mut(terminal)?;
        if matches!(
            ownership.state,
            TerminalState::Available
                | TerminalState::OrphanRunning
                | TerminalState::IdentityUnknown
        ) {
            return Err(GenerationError::TerminalUnavailable);
        }
        ownership.process = None;
        Ok(())
    }

    /// Forgets a resolved terminal whose durable runtime record is being
    /// deleted. Live ownership must be resolved first, so callers cannot use
    /// record cleanup as an unfenced process kill or capacity release.
    pub fn forget_terminal(&mut self, terminal: &TerminalRef) -> Result<(), GenerationError> {
        let key = terminal_key(terminal);
        let ownership = self
            .terminals
            .get(&key)
            .filter(|known| known.terminal.fences(terminal))
            .ok_or(GenerationError::TerminalOwnedElsewhere)?;
        if matches!(
            ownership.state,
            TerminalState::Available
                | TerminalState::OrphanRunning
                | TerminalState::IdentityUnknown
        ) {
            return Err(GenerationError::TerminalUnavailable);
        }
        self.terminals.remove(&key);
        Ok(())
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
    use usagi_core::domain::id::{SessionId, TerminalId, WorkspaceId, WorktreeId};

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
    fn restored_draining_owner_routes_its_terminal_and_fences_old_control() {
        let (mut registry, old) = coordinator();
        let pane = terminal(old);
        registry.reserve_terminal(pane.clone()).unwrap();
        registry.record_spawn(&pane, identity()).unwrap();
        let next = generation();
        let mut snapshot = registry.snapshot();
        snapshot.records[0].role = GenerationRole::Draining;
        snapshot.records.push(GenerationRecord {
            generation: next,
            endpoint: "next.sock".into(),
            role: GenerationRole::Active,
            expected_build: BuildIdentity::default(),
            build_verified: false,
        });
        snapshot.current = Some(next);
        let mut restored = GenerationCoordinator::restore(snapshot, 2).unwrap();
        assert_eq!(restored.terminal_endpoint(&pane), Ok("active.sock"));
        assert_eq!(
            restored.require_active(old),
            Err(GenerationError::NotActive)
        );
        assert_eq!(restored.require_active(next), Ok(()));
        assert_eq!(
            restored.register_standby(generation(), "third.sock".into()),
            Err(GenerationError::GenerationLimit)
        );
    }

    #[test]
    fn persisted_orphan_requires_exact_observation_before_release() {
        let (mut registry, owner) = coordinator();
        let pane = terminal(owner);
        registry.reserve_terminal(pane.clone()).unwrap();
        registry.record_spawn(&pane, identity()).unwrap();
        let mut snapshot = registry.snapshot();
        snapshot.terminals[0].state = TerminalState::OrphanRunning;
        let mut restored = GenerationCoordinator::restore(snapshot, 2).unwrap();
        assert_eq!(
            restored.terminal_endpoint(&pane),
            Err(GenerationError::TerminalUnavailable)
        );
        assert_eq!(
            restored.forget_resolved_process(&pane),
            Err(GenerationError::TerminalUnavailable)
        );
        assert_eq!(
            restored.forget_terminal(&pane),
            Err(GenerationError::TerminalUnavailable)
        );
        restored
            .resolve_orphan(&pane, ProcessObservation::VerifiedAlive(identity()), false)
            .unwrap();
        assert_eq!(
            restored.snapshot().terminals[0].state,
            TerminalState::OrphanRunning
        );
        assert_eq!(
            restored.resolve_orphan(&pane, ProcessObservation::Unknown, false),
            Err(GenerationError::TerminalUnavailable)
        );
        restored
            .resolve_orphan(&pane, ProcessObservation::VerifiedAlive(identity()), true)
            .unwrap();
        restored.forget_resolved_process(&pane).unwrap();
        assert_eq!(
            restored.snapshot().terminals[0].state,
            TerminalState::Terminated
        );
        assert!(restored.snapshot().terminals[0].process.is_none());
        restored.forget_terminal(&pane).unwrap();
        assert!(!restored.owns_terminal(&pane));
    }

    #[test]
    fn retained_legacy_build_metadata_round_trips_without_becoming_handoff_authority() {
        let (registry, _) = coordinator();
        let mut snapshot = registry.snapshot();
        snapshot.records[0].expected_build = usagi_core::infrastructure::ipc::build_identity(
            "2.6.0",
            "fixture",
            "test-target",
            "debug",
            &"a".repeat(64),
        );
        snapshot.records[0].build_verified = true;
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let restored =
            GenerationCoordinator::restore(serde_json::from_str(&encoded).unwrap(), 2).unwrap();
        assert_eq!(restored.snapshot(), snapshot);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One lifecycle fixture covers the coupled restore/reconcile edges.
    fn restore_reconcile_and_error_edges_fail_closed() {
        assert_eq!(
            GenerationCoordinator::new(2).activate_initial(generation()),
            Err(GenerationError::UnknownGeneration)
        );
        let standby = generation();
        let mut wrong_role = GenerationCoordinator::new(2);
        wrong_role
            .register_standby(standby, "standby.sock".into())
            .unwrap();
        wrong_role.records.get_mut(&standby.as_str()).unwrap().role = GenerationRole::Retired;
        assert_eq!(
            wrong_role.activate_initial(standby),
            Err(GenerationError::NotActive)
        );
        let (mut registry, active) = coordinator();
        let pane = terminal(active);
        registry.reserve_terminal(pane.clone()).unwrap();
        registry.record_spawn(&pane, identity()).unwrap();
        let snapshot = registry.snapshot();
        let restored = GenerationCoordinator::restore(snapshot.clone(), 2).unwrap();
        assert_eq!(restored.snapshot(), snapshot);
        let foreign_terminal = terminal(generation());
        assert_eq!(
            registry.terminal_endpoint(&foreign_terminal),
            Err(GenerationError::TerminalOwnedElsewhere)
        );
        assert_eq!(
            registry.reserve_terminal(foreign_terminal.clone()),
            Err(GenerationError::NotActive)
        );
        assert_eq!(
            registry.record_spawn(&foreign_terminal, identity()),
            Err(GenerationError::TerminalOwnedElsewhere)
        );
        assert_eq!(
            GenerationCoordinator::restore(
                GenerationSnapshot {
                    current: None,
                    records: Vec::new(),
                    terminals: vec![TerminalOwnership {
                        terminal: foreign_terminal,
                        process: None,
                        state: TerminalState::IdentityUnknown,
                    }],
                },
                2,
            )
            .unwrap_err(),
            GenerationError::UnknownGeneration
        );

        let mut duplicate_record = snapshot.clone();
        duplicate_record
            .records
            .push(duplicate_record.records[0].clone());
        assert_eq!(
            GenerationCoordinator::restore(duplicate_record, 2).unwrap_err(),
            GenerationError::DuplicateGeneration
        );
        let mut bad_current = snapshot.clone();
        bad_current.current = Some(generation());
        assert_eq!(
            GenerationCoordinator::restore(bad_current, 2).unwrap_err(),
            GenerationError::NotActive
        );
        let mut missing_current = snapshot.clone();
        missing_current.current = None;
        assert_eq!(
            GenerationCoordinator::restore(missing_current, 2).unwrap_err(),
            GenerationError::NotActive
        );
        let mut unavailable_owner = snapshot.clone();
        unavailable_owner.current = None;
        unavailable_owner.records[0].role = GenerationRole::Retired;
        assert_eq!(
            GenerationCoordinator::restore(unavailable_owner, 2).unwrap_err(),
            GenerationError::TerminalUnavailable
        );
        let mut missing_process = snapshot.clone();
        missing_process.terminals[0].process = None;
        assert_eq!(
            GenerationCoordinator::restore(missing_process, 2).unwrap_err(),
            GenerationError::TerminalUnavailable
        );
        let mut orphan_without_process = snapshot.clone();
        orphan_without_process.terminals[0].process = None;
        orphan_without_process.terminals[0].state = TerminalState::OrphanRunning;
        assert_eq!(
            GenerationCoordinator::restore(orphan_without_process, 2).unwrap_err(),
            GenerationError::TerminalUnavailable
        );
        let mut lost_without_process = snapshot.clone();
        lost_without_process.terminals[0].process = None;
        lost_without_process.terminals[0].state = TerminalState::Lost;
        assert!(GenerationCoordinator::restore(lost_without_process, 2).is_ok());
        let mut duplicate_terminal = snapshot;
        duplicate_terminal
            .terminals
            .push(duplicate_terminal.terminals[0].clone());
        assert_eq!(
            GenerationCoordinator::restore(duplicate_terminal, 2).unwrap_err(),
            GenerationError::DuplicateGeneration
        );

        assert_eq!(
            registry.register_standby(active, "duplicate.sock".into()),
            Err(GenerationError::DuplicateGeneration)
        );
        assert_eq!(
            registry.activate_initial(active),
            Err(GenerationError::NotActive)
        );
        assert_eq!(
            registry.reserve_terminal(pane.clone()),
            Err(GenerationError::TerminalOwnedElsewhere)
        );

        registry
            .resolve_orphan(&pane, ProcessObservation::Gone, false)
            .unwrap();
        registry
            .resolve_orphan(&pane, ProcessObservation::Unknown, true)
            .unwrap();
        let mut mismatched_identity = identity();
        mismatched_identity.pid += 1;
        assert_eq!(
            registry.resolve_orphan(
                &pane,
                ProcessObservation::VerifiedAlive(mismatched_identity),
                false
            ),
            Err(GenerationError::TerminalUnavailable)
        );

        let (mut orphaned, unknown_owner) = coordinator();
        let unknown_terminal = terminal(unknown_owner);
        orphaned.reserve_terminal(unknown_terminal.clone()).unwrap();
        orphaned
            .record_spawn(&unknown_terminal, identity())
            .unwrap();
        // Exercise the defensive route independently from restore(), which now
        // rejects this corrupted relationship before constructing a coordinator.
        orphaned.records.clear();
        assert_eq!(
            orphaned.terminal_endpoint(&unknown_terminal),
            Err(GenerationError::UnknownGeneration)
        );

        let (mut wrong_role, active) = coordinator();
        // restore() rejects this inconsistency; keep the defensive transition
        // covered by constructing it inside the module boundary.
        wrong_role.current = None;
        assert_eq!(
            wrong_role.activate_initial(active),
            Err(GenerationError::NotActive)
        );
    }
}

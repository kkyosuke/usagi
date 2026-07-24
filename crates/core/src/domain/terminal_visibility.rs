//! Workspace-global visibility of exited terminal/Agent tombstones.
//!
//! When a daemon-owned terminal (generic or Agent) exits, its bounded final
//! replay, exit status, and offsets are retained as a tombstone (the daemon
//! `Exited` record; see [terminal ownership](../../../document/05-daemon.md)).
//! This module publishes the *visibility* contract layered on top of that
//! retention: whether a fresh TUI should surface the tombstone as a completed,
//! read-only entry.
//!
//! Visibility is **not** client-local. It is authoritative, workspace-global
//! state keyed by the exact [`TerminalRef`] (daemon generation, terminal,
//! workspace, optional session, worktree). Every TUI process and every reopen
//! converges on the same result. The state is a monotonic lattice
//! `Unobserved < Observed < Dismissed` carrying a revision, mutated by
//! compare-and-swap so out-of-order and duplicated writes never lower the state
//! or resurrect a completed entry.
//!
//! This module owns only the visibility/observed/dismissed contract. Aggregate
//! retention and garbage collection of tombstones is delegated to #526, and
//! provider resume / replacement spawn is never triggered here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::{id::TerminalRef, terminal_launch::TerminalKind};

/// The monotonic visibility lattice for one exact terminal tombstone.
///
/// Ordering is defined by declaration order: `Unobserved < Observed <
/// Dismissed`. A state only ever rises; a merge takes the maximum of two
/// observations so a late or reordered write can never lower it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum TerminalVisibilityState {
    /// Never surfaced to any client. A fresh TUI may project it once.
    #[default]
    Unobserved,
    /// Surfaced to a client as a completed entry, but not yet dismissed.
    Observed,
    /// Explicitly closed by the user. It is never auto-shown again.
    Dismissed,
}

impl TerminalVisibilityState {
    /// Returns the higher (more-final) of two states.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        self.max(other)
    }
}

/// The authoritative visibility of one exact terminal tombstone: a lattice
/// state and the revision at which it was last raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalVisibility {
    pub state: TerminalVisibilityState,
    /// Monotonic revision. It increments only when [`state`](Self::state) is
    /// actually raised, so an idempotent retry does not perturb it.
    pub revision: u64,
}

impl Default for TerminalVisibility {
    fn default() -> Self {
        Self::unobserved()
    }
}

/// The result of applying a compare-and-swap visibility command.
///
/// Every variant carries the post-command authoritative snapshot so a caller
/// can merge it into local state. `Applied` and `Idempotent` are both success;
/// `Conflict` means the caller observed a stale revision and must merge the
/// returned snapshot and retry with its revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityOutcome {
    /// The state was raised. The snapshot carries the new, higher revision.
    Applied(TerminalVisibility),
    /// The requested state did not raise the current state (equal-or-lower
    /// retry, or a late write behind a higher state). Idempotent no-op.
    Idempotent(TerminalVisibility),
    /// A raise was required but the caller's `expected_revision` was stale. The
    /// snapshot is authoritative; merge to max-state and retry.
    Conflict(TerminalVisibility),
}

impl VisibilityOutcome {
    /// The authoritative post-command snapshot, regardless of variant.
    #[must_use]
    pub fn snapshot(self) -> TerminalVisibility {
        match self {
            Self::Applied(v) | Self::Idempotent(v) | Self::Conflict(v) => v,
        }
    }

    /// Whether the command succeeded (did not conflict).
    #[must_use]
    pub fn is_success(self) -> bool {
        !matches!(self, Self::Conflict(_))
    }
}

impl TerminalVisibility {
    /// The initial state of a tombstone no client has surfaced yet.
    #[must_use]
    pub const fn unobserved() -> Self {
        Self {
            state: TerminalVisibilityState::Unobserved,
            revision: 0,
        }
    }

    /// Raises this visibility toward `target` under compare-and-swap.
    ///
    /// If `target` does not exceed the current state the command is an
    /// idempotent no-op (this covers equal retries and late lower writes, so a
    /// stale `Observed` never lowers a `Dismissed`). Otherwise a raise requires
    /// `expected_revision` to match; a mismatch is a conflict that returns the
    /// authoritative snapshot without mutating.
    #[must_use]
    pub fn raise(
        self,
        target: TerminalVisibilityState,
        expected_revision: u64,
    ) -> VisibilityOutcome {
        let merged = self.state.merge(target);
        if merged == self.state {
            return VisibilityOutcome::Idempotent(self);
        }
        if expected_revision != self.revision {
            return VisibilityOutcome::Conflict(self);
        }
        VisibilityOutcome::Applied(Self {
            state: merged,
            revision: self.revision + 1,
        })
    }
}

/// One exited terminal tombstone in a scope query: its exact key, kind, exit
/// status, bounded replay locator, and workspace-global visibility.
///
/// It carries no argv, environment values, secrets, or provider transcript, so
/// it is safe to send on the wire. The exact [`TerminalRef`] is also the
/// retention identity; aggregate retention/GC is #526's responsibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedTerminalEntry {
    pub terminal: TerminalRef,
    pub kind: TerminalKind,
    /// The process exit status recorded when the PTY ended.
    pub exit_status: i32,
    /// The start of the retained replay window (a client resumes from here).
    pub base_offset: u64,
    /// The end offset of the final output (`base_offset + replay.len`).
    pub final_output_offset: u64,
    /// Authoritative workspace-global visibility for this exact tombstone.
    pub visibility: TerminalVisibility,
}

/// The authoritative, workspace-global visibility ledger for exited terminal
/// tombstones, keyed by the exact [`TerminalRef`].
///
/// It is pure state: the daemon owns one instance as the single authority all
/// TUI connections converge on, and persists it out-of-band. Distinct
/// `TerminalRef` incarnations have fully independent visibility, so raising one
/// never affects another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    into = "Vec<TerminalVisibilityRecord>",
    from = "Vec<TerminalVisibilityRecord>"
)]
pub struct TerminalVisibilityLedger {
    entries: BTreeMap<TerminalRef, TerminalVisibility>,
}

/// One persisted ledger row. JSON object keys must be strings, so the ledger is
/// stored as an explicit list of `{terminal, visibility}` records rather than a
/// map keyed by the structured [`TerminalRef`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalVisibilityRecord {
    pub terminal: TerminalRef,
    pub visibility: TerminalVisibility,
}

impl From<TerminalVisibilityLedger> for Vec<TerminalVisibilityRecord> {
    fn from(ledger: TerminalVisibilityLedger) -> Self {
        ledger
            .entries
            .into_iter()
            .map(|(terminal, visibility)| TerminalVisibilityRecord {
                terminal,
                visibility,
            })
            .collect()
    }
}

impl From<Vec<TerminalVisibilityRecord>> for TerminalVisibilityLedger {
    fn from(records: Vec<TerminalVisibilityRecord>) -> Self {
        Self {
            entries: records
                .into_iter()
                .map(|record| (record.terminal, record.visibility))
                .collect(),
        }
    }
}

impl TerminalVisibilityLedger {
    /// Creates an empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the authoritative visibility for `terminal`, defaulting to
    /// [`TerminalVisibility::unobserved`] for a tombstone never mutated.
    #[must_use]
    pub fn visibility(&self, terminal: &TerminalRef) -> TerminalVisibility {
        self.entries
            .get(terminal)
            .copied()
            .unwrap_or_else(TerminalVisibility::unobserved)
    }

    fn apply(
        &mut self,
        terminal: &TerminalRef,
        target: TerminalVisibilityState,
        expected_revision: u64,
    ) -> VisibilityOutcome {
        let outcome = self.visibility(terminal).raise(target, expected_revision);
        if let VisibilityOutcome::Applied(next) = outcome {
            self.entries.insert(terminal.clone(), next);
        }
        outcome
    }

    /// Raises `terminal` to at least `Observed` under compare-and-swap.
    pub fn observe(&mut self, terminal: &TerminalRef, expected_revision: u64) -> VisibilityOutcome {
        self.apply(
            terminal,
            TerminalVisibilityState::Observed,
            expected_revision,
        )
    }

    /// Raises `terminal` to `Dismissed` under compare-and-swap. A dismiss never
    /// mutates the terminal or its process; it only records visibility.
    pub fn dismiss(&mut self, terminal: &TerminalRef, expected_revision: u64) -> VisibilityOutcome {
        self.apply(
            terminal,
            TerminalVisibilityState::Dismissed,
            expected_revision,
        )
    }

    /// The number of tombstones with recorded (non-default) visibility.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger records no visibility mutations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId};

    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    #[test]
    fn state_lattice_orders_and_merges_to_the_maximum() {
        assert!(TerminalVisibilityState::Unobserved < TerminalVisibilityState::Observed);
        assert!(TerminalVisibilityState::Observed < TerminalVisibilityState::Dismissed);
        assert_eq!(
            TerminalVisibilityState::Observed.merge(TerminalVisibilityState::Unobserved),
            TerminalVisibilityState::Observed
        );
        assert_eq!(
            TerminalVisibilityState::Observed.merge(TerminalVisibilityState::Dismissed),
            TerminalVisibilityState::Dismissed
        );
        assert_eq!(
            TerminalVisibilityState::default(),
            TerminalVisibilityState::Unobserved
        );
    }

    #[test]
    fn raise_bumps_revision_only_when_state_rises() {
        let start = TerminalVisibility::unobserved();
        assert_eq!(TerminalVisibility::default(), start);
        let observed = start.raise(TerminalVisibilityState::Observed, 0);
        assert_eq!(
            observed,
            VisibilityOutcome::Applied(TerminalVisibility {
                state: TerminalVisibilityState::Observed,
                revision: 1,
            })
        );
        assert!(observed.is_success());
        // Equal retry is idempotent and does not perturb the revision.
        let again = observed
            .snapshot()
            .raise(TerminalVisibilityState::Observed, 1);
        assert_eq!(again, VisibilityOutcome::Idempotent(observed.snapshot()));
        assert!(again.is_success());
    }

    #[test]
    fn a_stale_observe_never_lowers_a_dismissed_state() {
        let dismissed = TerminalVisibility {
            state: TerminalVisibilityState::Dismissed,
            revision: 2,
        };
        // A late Observe with any expected revision is an idempotent no-op.
        let outcome = dismissed.raise(TerminalVisibilityState::Observed, 0);
        assert_eq!(outcome, VisibilityOutcome::Idempotent(dismissed));
        assert_eq!(outcome.snapshot().state, TerminalVisibilityState::Dismissed);
    }

    #[test]
    fn a_stale_raise_conflicts_and_returns_the_authoritative_snapshot() {
        let observed = TerminalVisibility {
            state: TerminalVisibilityState::Observed,
            revision: 1,
        };
        // Dismiss would raise, but expected revision 0 is stale.
        let outcome = observed.raise(TerminalVisibilityState::Dismissed, 0);
        assert_eq!(outcome, VisibilityOutcome::Conflict(observed));
        assert!(!outcome.is_success());
        // Merge and retry at the authoritative revision succeeds.
        let retried = outcome
            .snapshot()
            .raise(TerminalVisibilityState::Dismissed, 1);
        assert_eq!(
            retried,
            VisibilityOutcome::Applied(TerminalVisibility {
                state: TerminalVisibilityState::Dismissed,
                revision: 2,
            })
        );
    }

    #[test]
    fn ledger_defaults_to_unobserved_and_records_raises() {
        let mut ledger = TerminalVisibilityLedger::new();
        let a = terminal();
        let b = terminal();
        assert!(ledger.is_empty());
        assert_eq!(ledger.visibility(&a), TerminalVisibility::unobserved());

        let observed = ledger.observe(&a, 0);
        assert_eq!(
            observed,
            VisibilityOutcome::Applied(TerminalVisibility {
                state: TerminalVisibilityState::Observed,
                revision: 1,
            })
        );
        assert_eq!(ledger.len(), 1);
        // A distinct terminal's visibility is independent.
        assert_eq!(ledger.visibility(&b), TerminalVisibility::unobserved());

        let dismissed = ledger.dismiss(&a, 1);
        assert_eq!(
            dismissed.snapshot().state,
            TerminalVisibilityState::Dismissed
        );
        assert_eq!(dismissed.snapshot().revision, 2);
        assert_eq!(ledger.visibility(&a), dismissed.snapshot());
    }

    #[test]
    fn ledger_conflict_does_not_mutate_and_idempotent_retry_is_stable() {
        let mut ledger = TerminalVisibilityLedger::new();
        let t = terminal();
        assert_eq!(ledger.observe(&t, 0).snapshot().revision, 1);
        // Stale dismiss conflicts and leaves state unchanged.
        let conflict = ledger.dismiss(&t, 0);
        assert!(matches!(conflict, VisibilityOutcome::Conflict(_)));
        assert_eq!(
            ledger.visibility(&t).state,
            TerminalVisibilityState::Observed
        );
        // Idempotent lower retry leaves the ledger untouched.
        let idempotent = ledger.observe(&t, 99);
        assert_eq!(
            idempotent,
            VisibilityOutcome::Idempotent(ledger.visibility(&t))
        );
    }

    #[test]
    fn completed_entry_and_ledger_round_trip_through_json() {
        let mut ledger = TerminalVisibilityLedger::new();
        let t = terminal();
        ledger.observe(&t, 0);
        let entry = CompletedTerminalEntry {
            terminal: t.clone(),
            kind: TerminalKind::Agent,
            exit_status: 0,
            base_offset: 4,
            final_output_offset: 68,
            visibility: ledger.visibility(&t),
        };
        let encoded = serde_json::to_string(&entry).unwrap();
        assert!(encoded.contains("\"observed\""));
        let decoded: CompletedTerminalEntry = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, entry);

        let ledger_json = serde_json::to_string(&ledger).unwrap();
        let restored: TerminalVisibilityLedger = serde_json::from_str(&ledger_json).unwrap();
        assert_eq!(restored, ledger);
    }
}

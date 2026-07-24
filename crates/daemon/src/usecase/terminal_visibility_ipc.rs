//! Daemon-authoritative, workspace-global visibility of exited terminal
//! tombstones (#525).
//!
//! The generic terminal owner and the Agent owner each retain exited records
//! and their bounded final replay. This service holds the single
//! [`TerminalVisibilityLedger`] all client connections converge on, so every
//! TUI process and every reopen observes the same `Unobserved / Observed /
//! Dismissed` result for an exact [`TerminalRef`]. It is shared behind an
//! `Arc<Mutex<_>>` because `SharedTerminalOwner` is constructed per connection
//! while this authority must outlive any one connection.
//!
//! Aggregate retention and garbage collection of tombstones (and cross-restart
//! persistence) is #526's responsibility; this service publishes only the
//! observe / dismiss / visibility contract.

use std::sync::{Arc, Mutex, PoisonError};

use usagi_core::domain::{
    id::TerminalRef,
    terminal_visibility::{
        CompletedTerminalEntry, TerminalVisibility, TerminalVisibilityLedger, VisibilityOutcome,
    },
};

/// A cheaply-clonable handle to the daemon's single visibility ledger. Cloning
/// shares the same authoritative state; each clone speaks for the same
/// workspace-global authority.
#[derive(Clone, Default)]
pub struct SharedTerminalVisibility(Arc<Mutex<TerminalVisibilityLedger>>);

impl SharedTerminalVisibility {
    /// Creates a new authority over an empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `body` under the ledger lock, recovering from a poisoned lock so a
    /// panic on one connection never wedges visibility for the others.
    fn with<R>(&self, body: impl FnOnce(&mut TerminalVisibilityLedger) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        body(&mut guard)
    }

    /// Returns the authoritative visibility of `terminal`.
    #[must_use]
    pub fn visibility(&self, terminal: &TerminalRef) -> TerminalVisibility {
        self.with(|ledger| ledger.visibility(terminal))
    }

    /// Raises `terminal` to at least `Observed` under compare-and-swap.
    #[must_use]
    pub fn observe(&self, terminal: &TerminalRef, expected_revision: u64) -> VisibilityOutcome {
        self.with(|ledger| ledger.observe(terminal, expected_revision))
    }

    /// Raises `terminal` to `Dismissed` under compare-and-swap.
    #[must_use]
    pub fn dismiss(&self, terminal: &TerminalRef, expected_revision: u64) -> VisibilityOutcome {
        self.with(|ledger| ledger.dismiss(terminal, expected_revision))
    }

    /// Overwrites each entry's placeholder visibility with the authoritative
    /// workspace-global value.
    pub fn stamp(&self, entries: &mut [CompletedTerminalEntry]) {
        self.with(|ledger| {
            for entry in entries.iter_mut() {
                entry.visibility = ledger.visibility(&entry.terminal);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::{
        id::{DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId},
        terminal_launch::TerminalKind,
        terminal_visibility::TerminalVisibilityState,
    };

    fn terminal() -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        }
    }

    fn entry(terminal: TerminalRef) -> CompletedTerminalEntry {
        CompletedTerminalEntry {
            terminal,
            kind: TerminalKind::Terminal,
            exit_status: 0,
            base_offset: 0,
            final_output_offset: 8,
            visibility: TerminalVisibility::unobserved(),
        }
    }

    #[test]
    fn shared_authority_converges_across_clones() {
        let a = SharedTerminalVisibility::new();
        let b = a.clone();
        let t = terminal();
        assert_eq!(a.observe(&t, 0).snapshot().revision, 1);
        // A second clone observes the mutation performed through the first.
        assert_eq!(b.visibility(&t).state, TerminalVisibilityState::Observed);
        assert!(b.dismiss(&t, 1).is_success());
        assert_eq!(a.visibility(&t).state, TerminalVisibilityState::Dismissed);
    }

    #[test]
    fn stamp_overwrites_placeholder_visibility_from_the_ledger() {
        let visibility = SharedTerminalVisibility::new();
        let t = terminal();
        let _ = visibility.observe(&t, 0);
        let mut entries = vec![entry(t.clone()), entry(terminal())];
        visibility.stamp(&mut entries);
        assert_eq!(
            entries[0].visibility.state,
            TerminalVisibilityState::Observed
        );
        // A terminal with no recorded visibility stays Unobserved.
        assert_eq!(
            entries[1].visibility.state,
            TerminalVisibilityState::Unobserved
        );
    }

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_propagated() {
        let visibility = SharedTerminalVisibility::new();
        let t = terminal();
        let _ = visibility.observe(&t, 0);
        // Poison the inner lock by panicking while it is held.
        let poisoned = visibility.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoned.0.lock().unwrap();
            panic!("poison the visibility lock");
        }));
        assert!(visibility.0.is_poisoned());
        // The authority still answers, recovering the inner ledger.
        assert_eq!(
            visibility.visibility(&t).state,
            TerminalVisibilityState::Observed
        );
    }
}

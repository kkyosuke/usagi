//! Read-only projection of exited terminal tombstones into completed tabs
//! (#525).
//!
//! The daemon retains an exited terminal/Agent as a tombstone and publishes it
//! through the [`completed_inventory`] query with a workspace-global visibility
//! (`Unobserved / Observed / Dismissed` and a revision). This pure reducer
//! decides which tombstones a fresh TUI surfaces as **completed, read-only**
//! tabs, and which visibility commands to send, so the projection converges the
//! same way across every TUI process and reopen.
//!
//! [`completed_inventory`]: ../../../../document/04-ipc.md
//!
//! Rules, mirroring the [4. IPC](../../../../document/04-ipc.md) and
//! [3. TUI](../../../../document/03-tui.md) contracts:
//!
//! * An `Unobserved` tombstone auto-shows **once** and is marked `Observe`d, so
//!   a later inventory refresh (now `Observed`) does not re-show or re-notify.
//! * A `Dismissed` tombstone never auto-shows.
//! * A tombstone whose #506 continuation lineage is suppressed never auto-shows
//!   even while `Unobserved`; it is retained and reachable only by an explicit
//!   history reopen.
//! * An explicit reopen shows the exact tombstone read-only regardless of state
//!   or #506 suppression, without un-dismissing it or resuming any runtime.
//! * Distinct `TerminalRef` incarnations are independent; a completed tab sends
//!   no input, resize, resume, or spawn.

use std::collections::BTreeSet;

use usagi_core::domain::{
    id::TerminalRef,
    terminal_launch::TerminalKind,
    terminal_visibility::{CompletedTerminalEntry, TerminalVisibilityState},
};

/// A completed, read-only tab projected from an exited tombstone. It carries
/// only display data and a bounded final-replay locator; it has no
/// subscription, input, resize, resume, or spawn affordance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedTab {
    /// The exact tombstone identity (also its dedup key).
    pub terminal: TerminalRef,
    pub kind: TerminalKind,
    pub exit_status: i32,
    /// Start of the retained final-replay window (a read-only viewer resumes
    /// from here through the daemon's replay locator).
    pub base_offset: u64,
    /// End offset of the final output.
    pub final_output_offset: u64,
    /// Whether this tab was shown by an explicit history reopen rather than an
    /// automatic first-time projection.
    pub reopened: bool,
}

/// A compare-and-swap visibility command the caller sends to the daemon. The
/// `expected_revision` is the revision the projection observed; a daemon
/// conflict returns the authoritative snapshot to merge and retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityCommand {
    pub terminal: TerminalRef,
    pub expected_revision: u64,
}

/// The outcome of one projection pass over the current completed inventory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompletedProjection {
    /// The read-only tabs to display, in inventory order and deduped by exact
    /// `TerminalRef`.
    pub tabs: Vec<CompletedTab>,
    /// `Observe` commands for tombstones surfaced for the first time, so the
    /// workspace-global state records that they have been seen once.
    pub observe: Vec<VisibilityCommand>,
}

fn tab(entry: &CompletedTerminalEntry, reopened: bool) -> CompletedTab {
    CompletedTab {
        terminal: entry.terminal.clone(),
        kind: entry.kind,
        exit_status: entry.exit_status,
        base_offset: entry.base_offset,
        final_output_offset: entry.final_output_offset,
        reopened,
    }
}

/// Projects the current completed inventory into read-only tabs and the
/// visibility commands to issue.
///
/// * `suppressed` are the exact terminals whose #506 continuation lineage
///   forbids auto-show. The mapping from a continuation to its terminal is
///   owned by #506; this reducer never infers it.
/// * `reopened` are the exact terminals the user explicitly reopened from
///   history.
///
/// The result is a pure function of its inputs, so re-projecting after merging a
/// CAS-conflict snapshot into `entries` converges monotonically.
#[must_use]
pub fn project(
    entries: &[CompletedTerminalEntry],
    suppressed: &BTreeSet<TerminalRef>,
    reopened: &BTreeSet<TerminalRef>,
) -> CompletedProjection {
    let mut projection = CompletedProjection::default();
    let mut seen = BTreeSet::new();
    for entry in entries {
        // Exact-equal duplicate inventory rows collapse to one tab.
        if !seen.insert(entry.terminal.clone()) {
            continue;
        }
        let is_reopened = reopened.contains(&entry.terminal);
        let is_suppressed = suppressed.contains(&entry.terminal);
        match entry.visibility.state {
            TerminalVisibilityState::Unobserved => {
                // #506 suppression forbids auto-show; leave it Unobserved so a
                // later explicit reopen can still surface it read-only.
                if is_suppressed && !is_reopened {
                    continue;
                }
                projection.tabs.push(tab(entry, is_reopened));
                projection.observe.push(VisibilityCommand {
                    terminal: entry.terminal.clone(),
                    expected_revision: entry.visibility.revision,
                });
            }
            // Already surfaced once: only an explicit reopen shows it again, and
            // it is not re-observed.
            TerminalVisibilityState::Observed | TerminalVisibilityState::Dismissed => {
                if is_reopened {
                    projection.tabs.push(tab(entry, true));
                }
            }
        }
    }
    projection
}

/// Builds the compare-and-swap command that dismisses `entry` at the revision
/// the caller observed. Dismiss records visibility only; it never mutates the
/// terminal or its process.
#[must_use]
pub fn dismiss_command(entry: &CompletedTerminalEntry) -> VisibilityCommand {
    VisibilityCommand {
        terminal: entry.terminal.clone(),
        expected_revision: entry.visibility.revision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::{
        id::{DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId},
        terminal_visibility::TerminalVisibility,
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

    fn entry(
        terminal: TerminalRef,
        state: TerminalVisibilityState,
        revision: u64,
    ) -> CompletedTerminalEntry {
        CompletedTerminalEntry {
            terminal,
            kind: TerminalKind::Agent,
            exit_status: 0,
            base_offset: 0,
            final_output_offset: 16,
            visibility: TerminalVisibility { state, revision },
        }
    }

    #[test]
    fn an_unobserved_tombstone_auto_shows_once_and_is_observed() {
        let t = terminal();
        let entries = vec![entry(t.clone(), TerminalVisibilityState::Unobserved, 0)];
        let projection = project(&entries, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(projection.tabs.len(), 1);
        assert!(projection.tabs[0].terminal.fences(&t));
        assert!(!projection.tabs[0].reopened);
        assert_eq!(projection.tabs[0].kind, TerminalKind::Agent);
        assert_eq!(
            projection.observe,
            vec![VisibilityCommand {
                terminal: t.clone(),
                expected_revision: 0,
            }]
        );

        // After the observe is applied (state -> Observed), a refresh does not
        // auto-show or re-observe it.
        let observed = vec![entry(t, TerminalVisibilityState::Observed, 1)];
        let refreshed = project(&observed, &BTreeSet::new(), &BTreeSet::new());
        assert!(refreshed.tabs.is_empty());
        assert!(refreshed.observe.is_empty());
    }

    #[test]
    fn a_dismissed_tombstone_never_auto_shows() {
        let entries = vec![entry(terminal(), TerminalVisibilityState::Dismissed, 2)];
        let projection = project(&entries, &BTreeSet::new(), &BTreeSet::new());
        assert!(projection.tabs.is_empty());
        assert!(projection.observe.is_empty());
    }

    #[test]
    fn a_506_suppressed_tombstone_never_auto_shows_but_reopens_read_only() {
        let t = terminal();
        let entries = vec![entry(t.clone(), TerminalVisibilityState::Unobserved, 0)];
        let suppressed = BTreeSet::from([t.clone()]);

        // Auto-projection skips it and does not observe it, so it stays reachable.
        let auto = project(&entries, &suppressed, &BTreeSet::new());
        assert!(auto.tabs.is_empty());
        assert!(auto.observe.is_empty());

        // An explicit reopen surfaces it read-only even while suppressed.
        let reopened = BTreeSet::from([t.clone()]);
        let opened = project(&entries, &suppressed, &reopened);
        assert_eq!(opened.tabs.len(), 1);
        assert!(opened.tabs[0].reopened);
        assert!(opened.tabs[0].terminal.fences(&t));
    }

    #[test]
    fn explicit_reopen_of_a_dismissed_tombstone_shows_read_only_without_reobserving() {
        let t = terminal();
        let entries = vec![entry(t.clone(), TerminalVisibilityState::Dismissed, 3)];
        let reopened = BTreeSet::from([t.clone()]);
        let projection = project(&entries, &BTreeSet::new(), &reopened);
        assert_eq!(projection.tabs.len(), 1);
        assert!(projection.tabs[0].reopened);
        // It stays dismissed: no observe command is emitted by a reopen.
        assert!(projection.observe.is_empty());
    }

    #[test]
    fn distinct_incarnations_are_independent_and_duplicates_collapse() {
        let shown = terminal();
        let dismissed = terminal();
        let entries = vec![
            entry(shown.clone(), TerminalVisibilityState::Unobserved, 0),
            entry(dismissed.clone(), TerminalVisibilityState::Dismissed, 1),
            // An exact-equal duplicate inventory row for the first terminal.
            entry(shown.clone(), TerminalVisibilityState::Unobserved, 0),
        ];
        let projection = project(&entries, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(projection.tabs.len(), 1);
        assert!(projection.tabs[0].terminal.fences(&shown));
        assert_eq!(projection.observe.len(), 1);
    }

    #[test]
    fn a_cas_conflict_snapshot_merges_and_reprojection_converges() {
        // The projection issued an Observe at revision 0, but another TUI had
        // already dismissed the tombstone, so the daemon returns a Dismissed
        // snapshot at a higher revision. Merging it and re-projecting converges
        // to no auto-show and no further command.
        let t = terminal();
        let first = project(
            &[entry(t.clone(), TerminalVisibilityState::Unobserved, 0)],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert_eq!(first.observe.len(), 1);

        let authoritative = vec![entry(t, TerminalVisibilityState::Dismissed, 4)];
        let merged = project(&authoritative, &BTreeSet::new(), &BTreeSet::new());
        assert!(merged.tabs.is_empty());
        assert!(merged.observe.is_empty());
    }

    #[test]
    fn dismiss_command_targets_the_exact_ref_at_the_observed_revision() {
        let t = terminal();
        let entry = entry(t.clone(), TerminalVisibilityState::Observed, 1);
        assert_eq!(
            dismiss_command(&entry),
            VisibilityCommand {
                terminal: t,
                expected_revision: 1,
            }
        );
    }

    #[test]
    fn completed_tab_and_command_expose_only_display_data() {
        // A completed tab is data-only; exercising its derives documents that it
        // carries no input/resize/resume affordance.
        let t = terminal();
        let projection = project(
            &[entry(t, TerminalVisibilityState::Unobserved, 0)],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        let tab = projection.tabs[0].clone();
        assert_eq!(tab, projection.tabs[0]);
        assert!(!format!("{tab:?}").is_empty());
        assert!(!format!("{:?}", projection.observe[0]).is_empty());
        assert_eq!(
            CompletedProjection::default(),
            CompletedProjection::default()
        );
    }
}

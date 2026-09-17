//! Session-local editor state; daemon snapshots remain the progress authority.

use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
use usagi_core::domain::settings::AvailableModels;
use usagi_core::domain::workflow::{FinishedRun, Recipient, WorkflowCommand, WorkflowRun};

use super::environment_source::EnvironmentSourceEditor;

/// Whether the pane has ever received daemon-owned progress.
///
/// Only the first read is announced. The pane re-reads on a steady cadence, and
/// letting every one of those replace the status it just fetched made the
/// header flicker between two strings for as long as the tab stayed open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkflowFreshness {
    /// No snapshot has landed yet.
    #[default]
    Pending,
    /// At least one snapshot has landed.
    Observed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowPanel {
    pub agents: usagi_core::domain::workflow::WorkflowAgents,
    /// None focuses the goal; 0..3 select planner, implementer and reviewer.
    pub agent_field: Option<usize>,
    pub agents_edited: bool,
    pub run: Option<WorkflowRun>,
    /// Runs this session already finished, oldest first.
    pub finished: Vec<FinishedRun>,
    pub draft: EnvironmentSourceEditor,
    pub recipient: Option<Recipient>,
    pub error: Option<String>,
    pub loading: bool,
    /// Set to `Observed` once any snapshot has been applied.
    pub freshness: WorkflowFreshness,
    /// Frame tick the next background snapshot read may start on. Spacing the
    /// reads from the completion of the previous one keeps this lane
    /// single-flight and off the frame rate.
    pub snapshot_due_tick: u64,
    pub submitting: bool,
    /// Rows hidden below the viewport, counted from the newest row.
    ///
    /// Zero is "following the latest". It is bounded by [`WorkflowPanel::history_rows`]:
    /// an unbounded offset scrolls the window clean off the top of the history
    /// and leaves the pane blank with nothing on screen saying why.
    pub history_offset: usize,
    /// Row count of the last rendered history, so an offset the person set can
    /// be held against arriving rows instead of drifting forward under them.
    pub observed_history_rows: usize,
    pub pending: Option<(OperationId, WorkflowCommand)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowJob {
    pub workspace: WorkspaceId,
    pub session: SessionId,
    /// None is a read; Some retains the exact control payload across retries.
    pub control: Option<(OperationId, WorkflowCommand)>,
}

pub trait WorkflowPort {
    fn dispatch(&mut self, job: WorkflowJob, completions: super::daemon_backend::Completions);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowError {
    pub message: String,
    /// A lost final response must retry the same operation, never mint a second run.
    pub unconfirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowEdit {
    Start,
    End,
    Delete,
    /// Return the history to its newest row in one operation.
    HistoryLatest,
}

impl WorkflowPanel {
    #[must_use]
    pub fn recipient_label(&self) -> String {
        let agents = self.run.as_ref().map_or(self.agents, |run| run.agents);
        match self.recipient.unwrap_or(Recipient::Automatic) {
            Recipient::Automatic => "Automatic (current owner)".into(),
            Recipient::Implementer => format!("{} (implementation)", agents.implementer.selector()),
            Recipient::Reviewer => format!("{} (review)", agents.reviewer.selector()),
        }
    }

    pub fn cycle_recipient(&mut self) {
        if self.run.is_none() {
            if self.pending.is_none() && !self.submitting {
                self.agent_field = match self.agent_field {
                    None => Some(0),
                    Some(0) => Some(1),
                    Some(1) => Some(2),
                    _ => None,
                };
            }
            return;
        }
        self.recipient = Some(match self.recipient.unwrap_or(Recipient::Automatic) {
            Recipient::Automatic => Recipient::Implementer,
            Recipient::Implementer => Recipient::Reviewer,
            Recipient::Reviewer => Recipient::Automatic,
        });
    }

    /// Move one provider slot to the next installed CLI.
    ///
    /// The cycle offers `available` rather than the closed vocabulary: a
    /// provider that is not installed — or whose credential is not configured —
    /// would otherwise be selectable here and refused at launch.
    pub fn cycle_agent(&mut self, forward: bool, available: AvailableModels) {
        if self.run.is_some() || self.pending.is_some() || self.submitting {
            return;
        }
        let choices = available.iter().collect::<Vec<_>>();
        if choices.is_empty() {
            return;
        }
        let selected = match self.agent_field {
            Some(0) => &mut self.agents.planner,
            Some(1) => &mut self.agents.implementer,
            Some(2) => &mut self.agents.reviewer,
            _ => return,
        };
        let index = choices
            .iter()
            .position(|value| value == selected)
            .unwrap_or(0);
        *selected = choices[(index + if forward { 1 } else { choices.len() - 1 }) % choices.len()];
        self.agents_edited = true;
    }

    /// Keep the still-editable selection to providers this machine can launch.
    ///
    /// A started run — or a start already submitted — keeps what it was started
    /// with: the pane then reports what is running, not what could be picked.
    /// The guard matches [`cycle_agent`](Self::cycle_agent) rather than relying
    /// on every submitting panel also holding its `pending` command.
    pub fn restrict_agents(&mut self, available: AvailableModels) {
        if self.run.is_some() || self.pending.is_some() || self.submitting {
            return;
        }
        self.agents = self.agents.restricted_to(available);
    }

    /// How many rows [`crate::presentation::views::workflow`] draws for history.
    ///
    /// Scrolling is bounded by this, and `views::workflow` has a test asserting
    /// the two agree — a count that drifts from the drawn rows would put the
    /// bound back out of step with the window it is supposed to bound.
    #[must_use]
    pub fn history_rows(&self) -> usize {
        self.finished.len()
            + self
                .run
                .as_ref()
                .map_or(0, |run| run.history.len() + run.instructions.len())
    }

    /// The furthest back the history can be scrolled: one row always stays on
    /// screen, so paging up can never empty the pane.
    #[must_use]
    fn max_history_offset(&self) -> usize {
        self.history_rows().saturating_sub(1)
    }

    /// Page the history, keeping the offset inside its bounds.
    pub fn scroll_history(&mut self, back: bool) {
        const PAGE: usize = 5;
        self.history_offset = if back {
            self.history_offset
                .saturating_add(PAGE)
                .min(self.max_history_offset())
        } else {
            self.history_offset.saturating_sub(PAGE)
        };
    }

    /// Follow the newest row again.
    pub fn show_latest_history(&mut self) {
        self.history_offset = 0;
    }

    /// Hold a scrolled-back reader in place as rows arrive.
    ///
    /// The offset counts from the end, so rows appended under a non-zero offset
    /// would otherwise slide the window forward and move the text the person is
    /// reading. At offset zero the pane is following the latest and should.
    pub fn anchor_history(&mut self) {
        let rows = self.history_rows();
        if self.history_offset > 0 {
            let grown = rows.saturating_sub(self.observed_history_rows);
            self.history_offset = self
                .history_offset
                .saturating_add(grown)
                .min(self.max_history_offset());
        }
        self.observed_history_rows = rows;
    }

    /// Accept a successful submission without losing a later edit.
    ///
    /// `submitted_text` is `None` for a command that carried no draft, such as
    /// finishing a run: there is nothing the submission could have consumed.
    pub fn submitted(&mut self, submitted_text: Option<&str>) {
        self.submitting = false;
        self.error = None;
        if submitted_text == Some(self.draft.value()) {
            self.draft.replace("");
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture_run(session: SessionId) -> WorkflowRun {
    WorkflowRun {
        agents: usagi_core::domain::workflow::WorkflowAgents::default(),
        id: OperationId::new(),
        session,
        goal: "Implement login".into(),
        implementer: usagi_core::domain::id::AgentId::new(),
        reviewer: Some(usagi_core::domain::id::AgentId::new()),
        phase: usagi_core::domain::workflow::Phase::Implementing,
        revision_limit: 3,
        revisions: 0,
        review: None,
        waiting_reason: None,
        pr_url: None,
        issue: None,
        instructions: vec![],
        history: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_agent_selection_cycles_independently_and_freezes_pending_requests() {
        let all = AvailableModels::all();
        let mut panel = WorkflowPanel::default();
        let initial = panel.agents;
        panel.cycle_agent(true, all);
        assert_eq!(panel.agents, initial);
        for field in 0..3 {
            panel.cycle_recipient();
            assert_eq!(panel.agent_field, Some(field));
            panel.cycle_agent(true, all);
            assert_ne!(panel.agents, initial);
            panel.cycle_agent(false, all);
            assert_eq!(panel.agents, initial);
        }
        panel.cycle_recipient();
        assert_eq!(panel.agent_field, None);
        panel.cycle_recipient();
        panel.pending = Some((
            OperationId::new(),
            WorkflowCommand::Start {
                goal: "Task".into(),
                agents: panel.agents,
            },
        ));
        panel.cycle_agent(true, all);
        panel.cycle_recipient();
        assert_eq!(panel.agent_field, Some(0));
        assert_eq!(panel.agents, initial);
        panel.pending = None;
        panel.submitting = true;
        panel.cycle_agent(true, all);
        panel.cycle_recipient();
        assert_eq!(panel.agents, initial);
        panel.submitting = false;
        panel.run = Some(fixture_run(SessionId::new()));
        panel.cycle_agent(true, all);
        assert_eq!(panel.agents, initial);
    }

    #[test]
    fn only_launchable_providers_are_offered_and_kept() {
        use usagi_core::domain::settings::DefaultModel;
        use usagi_core::domain::workflow::WorkflowAgents;

        let claude_only = AvailableModels::new([DefaultModel::Claude]);
        let mut panel = WorkflowPanel::default();
        // The stored defaults name Codex, which this machine cannot launch.
        panel.restrict_agents(claude_only);
        assert_eq!(
            panel.agents,
            WorkflowAgents {
                planner: DefaultModel::Claude,
                implementer: DefaultModel::Claude,
                reviewer: DefaultModel::Claude,
            }
        );
        // One installed provider has nothing to cycle to, and none at all
        // leaves the slot alone rather than picking a refused launch.
        panel.cycle_recipient();
        panel.cycle_agent(true, claude_only);
        assert_eq!(panel.agents.planner, DefaultModel::Claude);
        assert!(panel.agents_edited);
        panel.cycle_agent(true, AvailableModels::default());
        assert_eq!(panel.agents.planner, DefaultModel::Claude);
        // With two installed providers the cycle visits exactly those two.
        let two = AvailableModels::new([DefaultModel::Claude, DefaultModel::Agy]);
        panel.cycle_agent(true, two);
        assert_eq!(panel.agents.planner, DefaultModel::Agy);
        panel.cycle_agent(true, two);
        assert_eq!(panel.agents.planner, DefaultModel::Claude);
        // A started run reports what it is running, restriction included.
        let started = WorkflowAgents::default();
        panel.agents = started;
        panel.run = Some(fixture_run(SessionId::new()));
        panel.restrict_agents(claude_only);
        assert_eq!(panel.agents, started);
        panel.run = None;
        panel.pending = Some((OperationId::new(), WorkflowCommand::Finish));
        panel.restrict_agents(claude_only);
        assert_eq!(panel.agents, started);
        panel.pending = None;
        panel.submitting = true;
        panel.restrict_agents(claude_only);
        assert_eq!(panel.agents, started);
    }

    fn panel_with_history(rows: usize) -> WorkflowPanel {
        use usagi_core::domain::workflow::WorkflowHistoryEntry;
        let mut run = fixture_run(SessionId::new());
        for index in 0..rows {
            run.history.push(WorkflowHistoryEntry {
                id: OperationId::new(),
                actor: "claude".into(),
                body: format!("entry {index}"),
            });
        }
        WorkflowPanel {
            run: Some(run),
            ..WorkflowPanel::default()
        }
    }

    #[test]
    fn paging_back_cannot_scroll_the_history_off_its_own_top() {
        let mut panel = panel_with_history(12);
        // Paging back past the oldest row used to drive `history_offset` past the
        // row count, which left the viewport empty with nothing explaining it.
        for _ in 0..20 {
            panel.scroll_history(true);
        }
        assert_eq!(panel.history_offset, 11);
        assert!(panel.history_offset < panel.history_rows());

        // One operation comes back to the newest row.
        panel.show_latest_history();
        assert_eq!(panel.history_offset, 0);

        // Paging forward from the latest stays there rather than underflowing.
        panel.scroll_history(false);
        assert_eq!(panel.history_offset, 0);

        // An empty history has nowhere to go.
        let mut empty = WorkflowPanel::default();
        empty.scroll_history(true);
        assert_eq!(empty.history_offset, 0);
    }

    #[test]
    fn arriving_rows_move_the_view_only_while_it_follows_the_latest() {
        let mut panel = panel_with_history(10);
        panel.anchor_history();
        assert_eq!(panel.observed_history_rows, 10);

        // Following the latest: new rows are what the reader wants to see.
        panel
            .run
            .as_mut()
            .expect("run")
            .history
            .extend(
                (0..3).map(|index| usagi_core::domain::workflow::WorkflowHistoryEntry {
                    id: OperationId::new(),
                    actor: "codex".into(),
                    body: format!("late {index}"),
                }),
            );
        panel.anchor_history();
        assert_eq!(panel.history_offset, 0);

        // Scrolled back: the offset counts from the end, so it has to grow by
        // exactly what arrived or the text under the reader slides forward.
        panel.scroll_history(true);
        let anchored = panel.history_offset;
        panel
            .run
            .as_mut()
            .expect("run")
            .history
            .extend(
                (0..2).map(|index| usagi_core::domain::workflow::WorkflowHistoryEntry {
                    id: OperationId::new(),
                    actor: "codex".into(),
                    body: format!("later {index}"),
                }),
            );
        panel.anchor_history();
        assert_eq!(panel.history_offset, anchored + 2);
        assert_eq!(panel.observed_history_rows, 15);

        // Rows disappearing (a finished run archived away) cannot push the
        // offset past the new bound.
        panel.history_offset = 14;
        panel.run.as_mut().expect("run").history.truncate(2);
        panel.anchor_history();
        assert_eq!(panel.history_offset, panel.history_rows() - 1);
    }

    #[test]
    fn recipient_cycle_and_ack_preserve_newer_drafts() {
        let mut panel = WorkflowPanel {
            run: Some(fixture_run(SessionId::new())),
            ..WorkflowPanel::default()
        };
        assert!(panel.recipient_label().contains("Automatic"));
        panel.cycle_recipient();
        assert!(panel.recipient_label().contains("codex"));
        panel.cycle_recipient();
        assert!(panel.recipient_label().contains("claude"));
        panel.cycle_recipient();
        assert!(panel.recipient_label().contains("Automatic"));
        panel.draft.replace("first\nsecond");
        panel.submitted(Some("first"));
        assert_eq!(panel.draft.value(), "first\nsecond");
        panel.submitted(Some("first\nsecond"));
        assert!(panel.draft.value().is_empty());
    }
}

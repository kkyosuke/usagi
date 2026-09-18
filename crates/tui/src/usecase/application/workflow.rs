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

/// Form field index of the revision-limit selector, after the three providers.
pub const REVISION_LIMIT_FIELD: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowPanel {
    pub agents: usagi_core::domain::workflow::WorkflowAgents,
    /// Revision rounds the next start takes before it asks for a person.
    pub revision_limit: u8,
    /// None focuses the goal; 0..=2 select planner, implementer and reviewer,
    /// and 3 selects the revision limit.
    pub agent_field: Option<usize>,
    /// Whether the person has changed anything in the start form. A background
    /// snapshot must not repaint a choice they just made, and that holds for the
    /// revision limit exactly as it does for the providers.
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
    /// Identity of the newest row the person can see while scrolled back.
    ///
    /// Row *counts* cannot anchor this: the daemon caps history at 100 entries
    /// and drops the oldest, so once a long run reaches the cap the count stops
    /// changing while the rows underneath keep moving. The identity does not.
    pub observed_anchor: Option<OperationId>,
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

impl Default for WorkflowPanel {
    fn default() -> Self {
        Self {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
            agent_field: None,
            agents_edited: false,
            run: None,
            finished: Vec::new(),
            draft: EnvironmentSourceEditor::default(),
            recipient: None,
            error: None,
            loading: false,
            freshness: WorkflowFreshness::default(),
            snapshot_due_tick: 0,
            submitting: false,
            history_offset: 0,
            observed_anchor: None,
            pending: None,
        }
    }
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
                    Some(2) => Some(REVISION_LIMIT_FIELD),
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
        // The limit shares the form's left/right cycle but not its vocabulary:
        // it is a number, and no provider has to be installed to pick one.
        if self.agent_field == Some(REVISION_LIMIT_FIELD) {
            let span = i16::from(usagi_core::domain::workflow::MAX_REVISION_LIMIT);
            let moved = i16::from(self.revision_limit) + if forward { 1 } else { -1 };
            self.revision_limit = u8::try_from(moved.clamp(1, span)).unwrap_or(self.revision_limit);
            self.agents_edited = true;
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

    /// Identity of every history row, in the order
    /// [`crate::presentation::views::workflow`] draws them.
    ///
    /// Scrolling is bounded by the length of this and anchored by its contents.
    /// `views::workflow` has a test asserting it matches the drawn rows one for
    /// one — a list that drifted from them would put both the bound and the
    /// anchor out of step with the window they describe.
    #[must_use]
    pub fn history_row_ids(&self) -> Vec<OperationId> {
        let mut ids = self
            .finished
            .iter()
            .map(|ended| ended.id)
            .collect::<Vec<_>>();
        if let Some(run) = &self.run {
            ids.extend(run.history.iter().map(|entry| entry.id));
            ids.extend(run.instructions.iter().map(|instruction| instruction.id));
        }
        ids
    }

    /// How many rows the history draws.
    #[must_use]
    pub fn history_rows(&self) -> usize {
        self.history_row_ids().len()
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
        self.remember_anchor();
    }

    /// Follow the newest row again.
    pub fn show_latest_history(&mut self) {
        self.history_offset = 0;
        self.remember_anchor();
    }

    /// Record which row the offset is currently measured against.
    ///
    /// Recorded the moment the person scrolls, not only when a snapshot lands:
    /// rows can arrive between the two, and an offset with nothing to hold would
    /// let exactly those rows slide the window.
    fn remember_anchor(&mut self) {
        self.observed_anchor = if self.history_offset > 0 {
            let ids = self.history_row_ids();
            ids.len()
                .checked_sub(self.history_offset + 1)
                .and_then(|index| ids.get(index).copied())
        } else {
            // Following the latest is not a position to hold.
            None
        };
    }

    /// Hold a scrolled-back reader in place as rows arrive.
    ///
    /// The offset counts from the end, so rows appended under a non-zero offset
    /// would otherwise slide the window forward and move the text the person is
    /// reading. At offset zero the pane is following the latest and should.
    ///
    /// The row the offset is measured against is remembered by identity. A count
    /// would hold only until the daemon's history cap starts evicting: past that
    /// the total stops growing while the contents keep shifting, and the window
    /// would slide again with nothing to notice it.
    pub fn anchor_history(&mut self) {
        let ids = self.history_row_ids();
        if self.history_offset > 0 {
            // No anchor means the person scrolled since the last snapshot: the
            // offset is already measured against these rows and needs no
            // correction, only the bound.
            if let Some(anchor) = self.observed_anchor {
                self.history_offset = ids.iter().position(|id| *id == anchor).map_or_else(
                    // The row being read has been evicted; the oldest
                    // retained row is as far back as the person can go.
                    || ids.len().saturating_sub(1),
                    |index| ids.len().saturating_sub(index + 1),
                );
            }
            self.history_offset = self.history_offset.min(self.max_history_offset());
        }
        self.remember_anchor();
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
        // The limit now sits between the last provider and the goal.
        panel.cycle_recipient();
        assert_eq!(panel.agent_field, Some(REVISION_LIMIT_FIELD));
        panel.cycle_recipient();
        assert_eq!(panel.agent_field, None);
        panel.cycle_recipient();
        panel.pending = Some((
            OperationId::new(),
            WorkflowCommand::Start {
                goal: "Task".into(),
                agents: panel.agents,
                revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
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
                at: None,
                kind: usagi_core::domain::agent_message::MessageKind::Message,
                advanced: false,
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
        // Following the latest, there is no row to hold on to.
        assert_eq!(panel.observed_anchor, None);

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
                    at: None,
                    kind: usagi_core::domain::agent_message::MessageKind::Message,
                    advanced: false,
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
                    at: None,
                    kind: usagi_core::domain::agent_message::MessageKind::Message,
                    advanced: false,
                }),
            );
        panel.anchor_history();
        assert_eq!(panel.history_offset, anchored + 2);
        assert!(panel.observed_anchor.is_some());

        // An offset set straight on the public field, with no anchor recorded
        // for it, is left where it was put and only bounded.
        panel.observed_anchor = None;
        panel.history_offset = 3;
        panel.anchor_history();
        assert_eq!(panel.history_offset, 3);

        // Rows disappearing (a finished run archived away) cannot push the
        // offset past the new bound.
        panel.history_offset = 14;
        panel.run.as_mut().expect("run").history.truncate(2);
        panel.anchor_history();
        assert_eq!(panel.history_offset, panel.history_rows() - 1);
    }

    #[test]
    fn the_anchor_holds_when_the_history_cap_starts_evicting() {
        use usagi_core::domain::workflow::WorkflowHistoryEntry;
        // A run at the daemon's retention cap: rows arrive and the same number
        // leave, so the total never changes. A count-based anchor sees "nothing
        // grew" and lets the window slide over the reader.
        let mut panel = panel_with_history(100);
        panel.scroll_history(true);
        panel.anchor_history();
        let watched = panel.observed_anchor.expect("a row is being read");
        let body = |panel: &WorkflowPanel, id| {
            panel
                .run
                .as_ref()
                .expect("run")
                .history
                .iter()
                .find(|entry| entry.id == id)
                .map(|entry| entry.body.clone())
        };
        let reading = body(&panel, watched).expect("the watched row is present");

        for index in 0..3 {
            let history = &mut panel.run.as_mut().expect("run").history;
            history.push(WorkflowHistoryEntry {
                id: OperationId::new(),
                actor: "codex".into(),
                body: format!("late {index}"),
                at: None,
                kind: usagi_core::domain::agent_message::MessageKind::Message,
                advanced: false,
            });
            history.remove(0);
        }
        assert_eq!(panel.history_rows(), 100, "the cap kept the total still");
        panel.anchor_history();
        // The offset moved by exactly the three rows that were evicted, so the
        // row being read is still the newest one on screen.
        assert_eq!(panel.observed_anchor, Some(watched));
        assert_eq!(body(&panel, watched).as_deref(), Some(reading.as_str()));

        // Once the watched row is itself evicted, the oldest retained row is as
        // far back as the person can go.
        let history = &mut panel.run.as_mut().expect("run").history;
        history.retain(|entry| entry.id != watched);
        panel.anchor_history();
        assert_eq!(panel.history_offset, panel.history_rows() - 1);
    }

    #[test]
    fn the_revision_limit_is_chosen_in_range_and_frozen_once_a_run_starts() {
        use usagi_core::domain::workflow::{DEFAULT_REVISION_LIMIT, MAX_REVISION_LIMIT};
        let all = AvailableModels::all();
        let mut panel = WorkflowPanel::default();
        assert_eq!(panel.revision_limit, DEFAULT_REVISION_LIMIT);

        // Tab reaches the limit after the three providers.
        for expected in [Some(0), Some(1), Some(2), Some(REVISION_LIMIT_FIELD), None] {
            panel.cycle_recipient();
            assert_eq!(panel.agent_field, expected);
        }
        panel.agent_field = Some(REVISION_LIMIT_FIELD);

        // Right walks up to the maximum and stops; left walks down to 1 and stops.
        for _ in 0..20 {
            panel.cycle_agent(true, all);
        }
        assert_eq!(panel.revision_limit, MAX_REVISION_LIMIT);
        for _ in 0..20 {
            panel.cycle_agent(false, all);
        }
        assert_eq!(panel.revision_limit, 1);
        assert!(panel.agents_edited);

        // No provider has to be installed to pick a number.
        panel.cycle_agent(true, AvailableModels::default());
        assert_eq!(panel.revision_limit, 2);

        // Picking the limit never disturbs the providers.
        assert_eq!(panel.agents, WorkflowPanel::default().agents);

        // A started run reports what it is running; the form stops accepting edits.
        panel.run = Some(fixture_run(SessionId::new()));
        panel.cycle_agent(true, all);
        assert_eq!(panel.revision_limit, 2);
        panel.run = None;
        panel.submitting = true;
        panel.cycle_agent(true, all);
        assert_eq!(panel.revision_limit, 2);
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

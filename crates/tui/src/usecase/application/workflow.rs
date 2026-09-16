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
    pub history_offset: usize,
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

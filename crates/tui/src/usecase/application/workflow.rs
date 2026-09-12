//! Session-local editor state; daemon snapshots remain the progress authority.

use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
use usagi_core::domain::workflow::{Recipient, WorkflowCommand, WorkflowRun};

use super::environment_source::EnvironmentSourceEditor;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowPanel {
    pub agents: usagi_core::domain::workflow::WorkflowAgents,
    /// None focuses the goal; 0..3 select planner, implementer and reviewer.
    pub agent_field: Option<usize>,
    pub agents_edited: bool,
    pub run: Option<WorkflowRun>,
    pub draft: EnvironmentSourceEditor,
    pub recipient: Option<Recipient>,
    pub error: Option<String>,
    pub loading: bool,
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

    pub fn cycle_agent(&mut self, forward: bool) {
        if self.run.is_some() || self.pending.is_some() || self.submitting {
            return;
        }
        let selected = match self.agent_field {
            Some(0) => &mut self.agents.planner,
            Some(1) => &mut self.agents.implementer,
            Some(2) => &mut self.agents.reviewer,
            _ => return,
        };
        let choices = usagi_core::domain::settings::DefaultModel::ALL;
        let index = choices
            .iter()
            .position(|value| value == selected)
            .unwrap_or(0);
        *selected = choices[(index + if forward { 1 } else { choices.len() - 1 }) % choices.len()];
        self.agents_edited = true;
    }

    /// Accept a successful submission without losing a later edit.
    pub fn submitted(&mut self, submitted_text: &str) {
        self.submitting = false;
        self.error = None;
        if self.draft.value() == submitted_text {
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
        let mut panel = WorkflowPanel::default();
        let initial = panel.agents;
        panel.cycle_agent(true);
        assert_eq!(panel.agents, initial);
        for field in 0..3 {
            panel.cycle_recipient();
            assert_eq!(panel.agent_field, Some(field));
            panel.cycle_agent(true);
            assert_ne!(panel.agents, initial);
            panel.cycle_agent(false);
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
        panel.cycle_agent(true);
        panel.cycle_recipient();
        assert_eq!(panel.agent_field, Some(0));
        assert_eq!(panel.agents, initial);
        panel.pending = None;
        panel.submitting = true;
        panel.cycle_agent(true);
        panel.cycle_recipient();
        assert_eq!(panel.agents, initial);
        panel.submitting = false;
        panel.run = Some(fixture_run(SessionId::new()));
        panel.cycle_agent(true);
        assert_eq!(panel.agents, initial);
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
        panel.submitted("first");
        assert_eq!(panel.draft.value(), "first\nsecond");
        panel.submitted("first\nsecond");
        assert!(panel.draft.value().is_empty());
    }
}

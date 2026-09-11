//! Session-local editor state; daemon snapshots remain the progress authority.

use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
use usagi_core::domain::workflow::{Recipient, WorkflowCommand, WorkflowRun};

use super::environment_source::EnvironmentSourceEditor;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowPanel {
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
    pub fn recipient_label(&self) -> &'static str {
        match self.recipient.unwrap_or(Recipient::Automatic) {
            Recipient::Automatic => "Automatic (current owner)",
            Recipient::Implementer => "Codex (implementation)",
            Recipient::Reviewer => "Claude (review)",
        }
    }

    pub fn cycle_recipient(&mut self) {
        self.recipient = Some(match self.recipient.unwrap_or(Recipient::Automatic) {
            Recipient::Automatic => Recipient::Implementer,
            Recipient::Implementer => Recipient::Reviewer,
            Recipient::Reviewer => Recipient::Automatic,
        });
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
        instructions: vec![],
        history: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_cycle_and_ack_preserve_newer_drafts() {
        let mut panel = WorkflowPanel::default();
        assert!(panel.recipient_label().contains("Automatic"));
        panel.cycle_recipient();
        assert!(panel.recipient_label().contains("Codex"));
        panel.cycle_recipient();
        assert!(panel.recipient_label().contains("Claude"));
        panel.cycle_recipient();
        assert!(panel.recipient_label().contains("Automatic"));
        panel.draft.replace("first\nsecond");
        panel.submitted("first");
        assert_eq!(panel.draft.value(), "first\nsecond");
        panel.submitted("first\nsecond");
        assert!(panel.draft.value().is_empty());
    }
}

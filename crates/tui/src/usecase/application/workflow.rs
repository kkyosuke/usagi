//! Session-local editor state; daemon snapshots remain the progress authority.

use usagi_core::domain::workflow::{Recipient, WorkflowRun};

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

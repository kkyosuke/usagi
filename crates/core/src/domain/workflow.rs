//! Session-local implementation/review progress. No Team or worktree ownership changes.

use serde::{Deserialize, Serialize};

use super::agent_message::ReviewTarget;
use super::id::{AgentId, OperationId, SessionId};

/// The participant chosen for an instruction. Automatic is resolved on admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recipient {
    Automatic,
    Implementer,
    Reviewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Implementing,
    Reviewing,
    Revising,
    Verifying,
    Ready,
    Waiting,
}

impl Phase {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Implementing => "Implementing",
            Self::Reviewing => "Reviewing",
            Self::Revising => "Revising",
            Self::Verifying => "Checking PR",
            Self::Ready => "PR ready",
            Self::Waiting => "Needs attention",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Queued,
    Notified,
    Acknowledged,
}

/// A durable instruction never changes recipient when the workflow advances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub id: OperationId,
    pub requested_recipient: Recipient,
    pub recipient: AgentId,
    pub body: String,
    pub delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub request: OperationId,
    pub target: ReviewTarget,
    pub approved: bool,
}

/// Stored by the daemon; opening or closing its UI does not create/stop a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: OperationId,
    pub session: SessionId,
    pub goal: String,
    pub implementer: AgentId,
    pub reviewer: AgentId,
    pub phase: Phase,
    pub revision_limit: u8,
    pub revisions: u8,
    pub review: Option<Review>,
    pub waiting_reason: Option<String>,
    pub instructions: Vec<Instruction>,
}

impl WorkflowRun {
    /// Reject malformed configuration before creating a durable run.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        valid_text(&self.goal)
            && self.implementer != self.reviewer
            && (1..=10).contains(&self.revision_limit)
            && self.revisions <= self.revision_limit
    }

    #[must_use]
    pub fn recipient(&self, recipient: Recipient) -> AgentId {
        match recipient {
            Recipient::Reviewer => self.reviewer,
            Recipient::Automatic if self.phase == Phase::Reviewing => self.reviewer,
            Recipient::Automatic | Recipient::Implementer => self.implementer,
        }
    }

    /// Idempotent retries retain the originally resolved exact participant.
    ///
    /// # Errors
    /// Rejects conflicting IDs, invalid text and a full instruction journal.
    pub fn enqueue(
        &mut self,
        id: OperationId,
        recipient: Recipient,
        body: String,
    ) -> Result<(), &'static str> {
        if let Some(previous) = self.instructions.iter().find(|item| item.id == id) {
            return if previous.body == body && previous.requested_recipient == recipient {
                Ok(())
            } else {
                Err("instruction ID conflicts with an existing instruction")
            };
        }
        if !valid_text(&body) || self.instructions.len() >= 100 {
            return Err("instruction is empty, invalid, or the journal is full");
        }
        self.instructions.push(Instruction {
            id,
            requested_recipient: recipient,
            recipient: self.recipient(recipient),
            body,
            delivery: Delivery::Queued,
        });
        Ok(())
    }

    /// Begin a review of immutable commits, never silently reuse an old approval.
    ///
    /// # Errors
    /// Rejects invalid targets and out-of-order submissions.
    pub fn request_review(
        &mut self,
        request: OperationId,
        target: ReviewTarget,
    ) -> Result<(), &'static str> {
        if !matches!(self.phase, Phase::Implementing | Phase::Revising) || !target.is_valid() {
            return Err("review requires an implementation and full commit SHAs");
        }
        if self
            .review
            .as_ref()
            .is_some_and(|old| old.request == request)
        {
            return Err("a new review requires a new request ID");
        }
        self.review = Some(Review {
            request,
            target,
            approved: false,
        });
        self.phase = Phase::Reviewing;
        Ok(())
    }

    /// Accept only the assigned reviewer's verdict for the exact current request.
    ///
    /// # Errors
    /// Rejects stale, duplicated and foreign verdicts without advancing progress.
    pub fn verdict(
        &mut self,
        from: AgentId,
        request: OperationId,
        target: &ReviewTarget,
        approved: bool,
    ) -> Result<(), &'static str> {
        if self.phase != Phase::Reviewing || from != self.reviewer {
            return Err("verdict is not from the active reviewer");
        }
        let review = self.review.as_mut().ok_or("no active review")?;
        if review.request != request || &review.target != target {
            return Err("verdict does not match the current review");
        }
        review.approved = approved;
        if approved {
            self.phase = Phase::Verifying;
        } else if self.revisions == self.revision_limit {
            self.phase = Phase::Waiting;
            self.waiting_reason = Some("Revision limit reached".into());
        } else {
            self.revisions += 1;
            self.phase = Phase::Revising;
        }
        Ok(())
    }

    /// PR preparation needs independent evidence for the approved HEAD.
    ///
    /// # Errors
    /// Rejects missing checks, absent PR readiness and approvals of older commits.
    pub fn mark_ready(
        &mut self,
        head: &str,
        checks_passed: bool,
        pr_ready: bool,
    ) -> Result<(), &'static str> {
        if self.phase != Phase::Verifying
            || !checks_passed
            || !pr_ready
            || !self
                .review
                .as_ref()
                .is_some_and(|review| review.approved && review.target.head_sha == head)
        {
            return Err("latest HEAD approval and successful PR checks are required");
        }
        self.phase = Phase::Ready;
        Ok(())
    }
}

fn valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= 16 * 1024 && !text.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> WorkflowRun {
        WorkflowRun {
            id: OperationId::new(),
            session: SessionId::new(),
            goal: "Implement authentication".into(),
            implementer: AgentId::new(),
            reviewer: AgentId::new(),
            phase: Phase::Implementing,
            revision_limit: 3,
            revisions: 0,
            review: None,
            waiting_reason: None,
            instructions: Vec::new(),
        }
    }

    fn target() -> ReviewTarget {
        ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        }
    }

    #[test]
    fn instructions_keep_exact_recipient_across_stage_changes_and_retries() {
        let mut run = run();
        let id = OperationId::new();
        run.enqueue(id, Recipient::Automatic, "Check errors".into())
            .unwrap();
        run.phase = Phase::Reviewing;
        run.enqueue(id, Recipient::Automatic, "Check errors".into())
            .unwrap();
        assert_eq!(run.instructions.len(), 1);
        assert_eq!(run.instructions[0].recipient, run.implementer);
        assert_eq!(run.instructions[0].delivery, Delivery::Queued);
        assert!(
            run.enqueue(id, Recipient::Reviewer, "Check errors".into())
                .is_err()
        );
        assert!(
            run.enqueue(id, Recipient::Automatic, "Changed".into())
                .is_err()
        );
        assert_eq!(run.recipient(Recipient::Automatic), run.reviewer);
        assert_eq!(run.recipient(Recipient::Reviewer), run.reviewer);
        assert_eq!(run.recipient(Recipient::Implementer), run.implementer);
        for body in [" ".to_owned(), "\0".into(), "x".repeat(16385)] {
            assert!(
                run.enqueue(OperationId::new(), Recipient::Automatic, body)
                    .is_err()
            );
        }
        while run.instructions.len() < 100 {
            run.enqueue(OperationId::new(), Recipient::Automatic, "Check".into())
                .unwrap();
        }
        assert!(
            run.enqueue(OperationId::new(), Recipient::Automatic, "Check".into())
                .is_err()
        );
    }

    #[test]
    fn approval_is_bound_to_request_reviewer_and_current_head() {
        let mut run = run();
        let request = OperationId::new();
        let target = target();
        assert!(run.mark_ready(&target.head_sha, true, true).is_err());
        assert!(run.verdict(run.reviewer, request, &target, true).is_err());
        run.request_review(request, target.clone()).unwrap();
        assert!(run.request_review(request, target.clone()).is_err());
        assert!(
            run.verdict(run.implementer, request, &target, true)
                .is_err()
        );
        assert!(
            run.verdict(run.reviewer, OperationId::new(), &target, true)
                .is_err()
        );
        let stale = ReviewTarget {
            head_sha: "c".repeat(40),
            ..target.clone()
        };
        assert!(run.verdict(run.reviewer, request, &stale, true).is_err());
        run.verdict(run.reviewer, request, &target, true).unwrap();
        for (head, checks, pr) in [
            (&stale.head_sha, true, true),
            (&target.head_sha, false, true),
            (&target.head_sha, true, false),
        ] {
            assert!(run.mark_ready(head, checks, pr).is_err());
        }
        run.mark_ready(&target.head_sha, true, true).unwrap();
        assert_eq!(run.phase, Phase::Ready);
    }

    #[test]
    fn revisions_are_bounded_and_old_requests_cannot_be_reused() {
        let mut run = run();
        let target = target();
        for round in 0..=3 {
            let request = OperationId::new();
            run.request_review(request, target.clone()).unwrap();
            run.verdict(run.reviewer, request, &target, false).unwrap();
            if round < 3 {
                assert_eq!(run.phase, Phase::Revising);
                assert!(run.request_review(request, target.clone()).is_err());
            }
        }
        assert_eq!(run.phase, Phase::Waiting);
        assert_eq!(run.revisions, 3);
        assert_eq!(
            run.waiting_reason.as_deref(),
            Some("Revision limit reached")
        );
    }

    #[test]
    fn validation_and_incomplete_state_fail_closed() {
        let mut run = run();
        assert!(run.is_valid());
        run.goal.clear();
        assert!(!run.is_valid());
        run.goal = "Task".into();
        run.reviewer = run.implementer;
        assert!(!run.is_valid());
        run.reviewer = AgentId::new();
        run.revision_limit = 0;
        assert!(!run.is_valid());
        run.revision_limit = 3;
        run.revisions = 4;
        assert!(!run.is_valid());
        run.revisions = 0;
        let invalid = ReviewTarget {
            head_sha: "main".into(),
            ..target()
        };
        assert!(run.request_review(OperationId::new(), invalid).is_err());
        run.phase = Phase::Reviewing;
        assert!(
            run.verdict(run.reviewer, OperationId::new(), &target(), true)
                .is_err()
        );
        run.phase = Phase::Verifying;
        assert!(run.mark_ready(&target().head_sha, true, true).is_err());
        for phase in [
            Phase::Implementing,
            Phase::Reviewing,
            Phase::Revising,
            Phase::Verifying,
            Phase::Ready,
            Phase::Waiting,
        ] {
            assert!(!phase.label().is_empty());
        }
    }
}

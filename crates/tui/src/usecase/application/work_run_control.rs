//! Pure Work Run control state machine.
//!
//! The daemon owns run truth and mutation. This module owns only the local
//! interaction: stable-ID selection, confirm-before-mutate, escalation choice,
//! and replaying the same operation after an unconfirmed response. It has no
//! terminal, thread, IPC, or rendering dependency.

use usagi_core::domain::id::OperationId;
use usagi_core::domain::id::WorkspaceId;
use usagi_core::domain::supervisor::{
    EscalationDecision, SupervisorRunId, SupervisorRunQuery, SupervisorRunState,
    SupervisorWorkspaceCommand, SupervisorWorkspaceSnapshot,
};

/// Safe message for every control transport whose durable outcome cannot be
/// established. The recovery action is always replaying the same operation.
pub const WORK_RUN_ACTION_UNCONFIRMED: &str =
    "Work Run action outcome is unconfirmed; retry the same operation";

/// Typed control failure keeps a definitive refusal distinct from an outcome
/// that may already have been committed. Only the latter retains and replays
/// the operation identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRunControlError {
    Rejected(String),
    Unconfirmed(String),
}

impl WorkRunControlError {
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Rejected(message) | Self::Unconfirmed(message) => message,
        }
    }
}

/// Serialized daemon boundary for one workspace's durable Work Runs.
///
/// Observation and mutation deliberately share one port so the application
/// shell cannot let a stale observation overtake a control result.
pub trait WorkRunPort: Send {
    /// Return a bounded, redaction-safe snapshot.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon cannot provide one coherent
    /// snapshot for the requested workspace.
    fn snapshot(&mut self, workspace: WorkspaceId) -> Result<SupervisorWorkspaceSnapshot, String>;

    /// Apply one typed, idempotent human command and return its safe run.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when authority, validation, persistence, or exact
    /// worker termination cannot be completed.
    fn control(
        &mut self,
        workspace: WorkspaceId,
        operation_id: OperationId,
        command: SupervisorWorkspaceCommand,
    ) -> Result<SupervisorRunQuery, WorkRunControlError>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkRunControlMode {
    #[default]
    Closed,
    List,
    ConfirmCancel,
    ResolveEscalation,
    Submitting,
    Retry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkRunControlAction {
    Toggle,
    Up,
    Down,
    PreviousDecision,
    NextDecision,
    Enter,
    Escape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRunControlRequest {
    pub operation_id: OperationId,
    pub command: SupervisorWorkspaceCommand,
    /// Fresh daemon revision observed before this exact command was created.
    /// A final reply must advance it; cached or merely admitted data is never
    /// enough to forget the replay identity.
    pub observed_state_revision: u64,
}

impl WorkRunControlRequest {
    fn accepts_result(&self, run: &SupervisorRunQuery) -> bool {
        if run.supervisor_run_id != self.command.supervisor_run_id()
            || run.state_revision <= self.observed_state_revision
            || run.escalation.is_some()
        {
            return false;
        }
        match &self.command {
            SupervisorWorkspaceCommand::Cancel { .. } => run.state == SupervisorRunState::Cancelled,
            SupervisorWorkspaceCommand::ResolveEscalation { decision, .. } => {
                run.state
                    == match decision {
                        EscalationDecision::Resume => SupervisorRunState::Running,
                        EscalationDecision::Cancel => SupervisorRunState::Cancelled,
                        EscalationDecision::Fail => SupervisorRunState::Failed,
                    }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkRunControlOutcome {
    Consumed,
    Submit(WorkRunControlRequest),
}

impl WorkRunControlOutcome {
    #[must_use]
    pub fn into_request(self) -> Option<WorkRunControlRequest> {
        match self {
            Self::Consumed => None,
            Self::Submit(request) => Some(request),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRunControl {
    mode: WorkRunControlMode,
    selected: Option<SupervisorRunId>,
    decision: EscalationDecision,
    escalation_fence: Option<OperationId>,
    feedback: Option<String>,
    retry: Option<WorkRunControlRequest>,
}

impl Default for WorkRunControl {
    fn default() -> Self {
        Self {
            mode: WorkRunControlMode::Closed,
            selected: None,
            decision: EscalationDecision::Resume,
            escalation_fence: None,
            feedback: None,
            retry: None,
        }
    }
}

impl WorkRunControl {
    #[must_use]
    pub const fn mode(&self) -> WorkRunControlMode {
        self.mode
    }

    #[must_use]
    pub const fn selected(&self) -> Option<SupervisorRunId> {
        self.selected
    }

    #[must_use]
    pub const fn decision(&self) -> EscalationDecision {
        self.decision
    }

    #[must_use]
    pub fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    pub fn sync_selection(&mut self, runs: &[SupervisorRunQuery]) {
        if self
            .selected
            .is_some_and(|selected| runs.iter().any(|run| run.supervisor_run_id == selected))
        {
            return;
        }
        if matches!(
            self.mode,
            WorkRunControlMode::Closed | WorkRunControlMode::List
        ) {
            self.selected = runs.first().map(|run| run.supervisor_run_id);
        }
    }

    pub fn handle(
        &mut self,
        action: WorkRunControlAction,
        runs: &[SupervisorRunQuery],
        fresh: bool,
    ) -> WorkRunControlOutcome {
        match self.mode {
            WorkRunControlMode::Closed => {
                if action == WorkRunControlAction::Toggle {
                    if self.retry.is_some() {
                        self.mode = WorkRunControlMode::Retry;
                        self.feedback = Some(WORK_RUN_ACTION_UNCONFIRMED.to_owned());
                    } else {
                        self.mode = WorkRunControlMode::List;
                        self.feedback = None;
                        self.sync_selection(runs);
                    }
                }
                WorkRunControlOutcome::Consumed
            }
            WorkRunControlMode::List => self.handle_list(action, runs, fresh),
            WorkRunControlMode::ConfirmCancel => self.handle_cancel(action, runs, fresh),
            WorkRunControlMode::ResolveEscalation => self.handle_decision(action, runs, fresh),
            WorkRunControlMode::Submitting => WorkRunControlOutcome::Consumed,
            WorkRunControlMode::Retry => self.handle_retry(action),
        }
    }

    /// Open the typed action for one Overview-selected run without routing
    /// through a second, duplicate run-list screen. The stable run identity is
    /// supplied by the Overview rail; an outstanding unconfirmed mutation keeps
    /// precedence so its idempotency key cannot be lost.
    pub fn open_action_for(
        &mut self,
        selected: SupervisorRunId,
        runs: &[SupervisorRunQuery],
        fresh: bool,
    ) -> WorkRunControlOutcome {
        if self.retry.is_some() {
            self.mode = WorkRunControlMode::Retry;
            self.feedback = Some(WORK_RUN_ACTION_UNCONFIRMED.to_owned());
            return WorkRunControlOutcome::Consumed;
        }
        self.selected = Some(selected);
        self.mode = WorkRunControlMode::List;
        self.feedback = None;
        self.open_selected_action(runs, fresh);
        WorkRunControlOutcome::Consumed
    }

    fn handle_list(
        &mut self,
        action: WorkRunControlAction,
        runs: &[SupervisorRunQuery],
        fresh: bool,
    ) -> WorkRunControlOutcome {
        match action {
            WorkRunControlAction::Toggle | WorkRunControlAction::Escape => self.close(),
            WorkRunControlAction::Up | WorkRunControlAction::PreviousDecision => {
                self.move_selection(runs, false);
            }
            WorkRunControlAction::Down | WorkRunControlAction::NextDecision => {
                self.move_selection(runs, true);
            }
            WorkRunControlAction::Enter => self.open_selected_action(runs, fresh),
        }
        WorkRunControlOutcome::Consumed
    }

    fn move_selection(&mut self, runs: &[SupervisorRunQuery], next: bool) {
        let Some(current) = self.selected else {
            self.sync_selection(runs);
            return;
        };
        let Some(index) = runs.iter().position(|run| run.supervisor_run_id == current) else {
            self.sync_selection(runs);
            return;
        };
        let index = if next {
            (index + 1).min(runs.len().saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
        self.selected = runs.get(index).map(|run| run.supervisor_run_id);
        self.feedback = None;
    }

    fn open_selected_action(&mut self, runs: &[SupervisorRunQuery], fresh: bool) {
        if !fresh {
            self.feedback = Some("Refresh Work Runs before changing one".to_owned());
            return;
        }
        let Some(run) = self
            .selected
            .and_then(|selected| runs.iter().find(|run| run.supervisor_run_id == selected))
        else {
            self.feedback = Some("No Work Run is selected".to_owned());
            return;
        };
        self.feedback = None;
        self.escalation_fence = None;
        if run.state.is_finished() {
            self.feedback = Some("This Work Run is already finished".to_owned());
        } else if run.state == SupervisorRunState::Escalated {
            if let Some(escalation) = &run.escalation {
                self.mode = WorkRunControlMode::ResolveEscalation;
                self.decision = EscalationDecision::Resume;
                self.escalation_fence = Some(escalation.escalation_id);
            } else {
                self.feedback = Some("This Work Run has no current decision".to_owned());
            }
        } else {
            self.mode = WorkRunControlMode::ConfirmCancel;
        }
    }

    fn handle_cancel(
        &mut self,
        action: WorkRunControlAction,
        runs: &[SupervisorRunQuery],
        fresh: bool,
    ) -> WorkRunControlOutcome {
        match action {
            WorkRunControlAction::Escape => {
                self.mode = WorkRunControlMode::List;
                WorkRunControlOutcome::Consumed
            }
            WorkRunControlAction::Enter => {
                if !fresh {
                    self.mode = WorkRunControlMode::List;
                    self.feedback = Some("Refresh Work Runs before changing one".to_owned());
                    return WorkRunControlOutcome::Consumed;
                }
                let Some(run) = self
                    .selected
                    .and_then(|selected| runs.iter().find(|run| run.supervisor_run_id == selected))
                else {
                    self.mode = WorkRunControlMode::List;
                    self.feedback = Some("The selected Work Run is no longer available".to_owned());
                    return WorkRunControlOutcome::Consumed;
                };
                if run.state.is_finished() || run.state == SupervisorRunState::Escalated {
                    self.mode = WorkRunControlMode::List;
                    self.feedback = Some("The Work Run action changed; review it again".to_owned());
                    return WorkRunControlOutcome::Consumed;
                }
                self.submit(
                    SupervisorWorkspaceCommand::Cancel {
                        supervisor_run_id: run.supervisor_run_id,
                        reason: "cancelled by local operator".to_owned(),
                    },
                    run.state_revision,
                )
            }
            WorkRunControlAction::Toggle
            | WorkRunControlAction::Up
            | WorkRunControlAction::Down
            | WorkRunControlAction::PreviousDecision
            | WorkRunControlAction::NextDecision => WorkRunControlOutcome::Consumed,
        }
    }

    fn handle_decision(
        &mut self,
        action: WorkRunControlAction,
        runs: &[SupervisorRunQuery],
        fresh: bool,
    ) -> WorkRunControlOutcome {
        match action {
            WorkRunControlAction::Escape => {
                self.mode = WorkRunControlMode::List;
                WorkRunControlOutcome::Consumed
            }
            WorkRunControlAction::Up | WorkRunControlAction::PreviousDecision => {
                self.decision = previous_escalation_decision(self.decision);
                WorkRunControlOutcome::Consumed
            }
            WorkRunControlAction::Down | WorkRunControlAction::NextDecision => {
                self.decision = next_escalation_decision(self.decision);
                WorkRunControlOutcome::Consumed
            }
            WorkRunControlAction::Enter => {
                if !fresh {
                    self.mode = WorkRunControlMode::List;
                    self.feedback = Some("Refresh Work Runs before changing one".to_owned());
                    return WorkRunControlOutcome::Consumed;
                }
                let escalation = self.selected.zip(self.escalation_fence).and_then(
                    |(selected, escalation_fence)| {
                        runs.iter()
                            .find(|run| {
                                run.supervisor_run_id == selected
                                    && run.state == SupervisorRunState::Escalated
                            })
                            .and_then(|run| {
                                run.escalation
                                    .as_ref()
                                    .filter(|escalation| {
                                        escalation.escalation_id == escalation_fence
                                    })
                                    .map(|escalation| {
                                        (selected, escalation.escalation_id, run.state_revision)
                                    })
                            })
                    },
                );
                if let Some((selected, escalation_id, state_revision)) = escalation {
                    self.submit(
                        SupervisorWorkspaceCommand::ResolveEscalation {
                            supervisor_run_id: selected,
                            escalation_id,
                            decision: self.decision,
                        },
                        state_revision,
                    )
                } else {
                    self.mode = WorkRunControlMode::List;
                    self.escalation_fence = None;
                    self.feedback = Some("The Work Run decision is no longer current".to_owned());
                    WorkRunControlOutcome::Consumed
                }
            }
            WorkRunControlAction::Toggle => WorkRunControlOutcome::Consumed,
        }
    }

    fn submit(
        &mut self,
        command: SupervisorWorkspaceCommand,
        observed_state_revision: u64,
    ) -> WorkRunControlOutcome {
        let request = WorkRunControlRequest {
            operation_id: OperationId::new(),
            command,
            observed_state_revision,
        };
        self.mode = WorkRunControlMode::Submitting;
        self.feedback = None;
        self.retry = Some(request.clone());
        WorkRunControlOutcome::Submit(request)
    }

    fn handle_retry(&mut self, action: WorkRunControlAction) -> WorkRunControlOutcome {
        match action {
            WorkRunControlAction::Escape => {
                // An unconfirmed durable effect may already have committed.
                // Closing may hide it, but must not discard the only operation
                // identity that can be replayed without duplicating the action.
                self.mode = WorkRunControlMode::Closed;
                self.feedback = None;
                self.escalation_fence = None;
                WorkRunControlOutcome::Consumed
            }
            WorkRunControlAction::Enter => {
                if let Some(request) = self.retry.clone() {
                    self.mode = WorkRunControlMode::Submitting;
                    WorkRunControlOutcome::Submit(request)
                } else {
                    self.mode = WorkRunControlMode::List;
                    self.feedback = Some("No Work Run action is available to retry".to_owned());
                    WorkRunControlOutcome::Consumed
                }
            }
            WorkRunControlAction::Toggle
            | WorkRunControlAction::Up
            | WorkRunControlAction::Down
            | WorkRunControlAction::PreviousDecision
            | WorkRunControlAction::NextDecision => WorkRunControlOutcome::Consumed,
        }
    }

    pub fn complete(
        &mut self,
        operation_id: OperationId,
        result: &Result<SupervisorRunQuery, WorkRunControlError>,
    ) -> bool {
        let Some(request) = self
            .retry
            .as_ref()
            .filter(|request| request.operation_id == operation_id)
        else {
            return false;
        };
        match result {
            Ok(run) if request.accepts_result(run) => {
                self.selected = Some(run.supervisor_run_id);
                self.mode = WorkRunControlMode::List;
                self.escalation_fence = None;
                self.feedback = Some("Work Run updated".to_owned());
                self.retry = None;
                true
            }
            Ok(_) => {
                self.mode = WorkRunControlMode::Retry;
                self.feedback = Some("daemon returned an invalid Work Run result".to_owned());
                false
            }
            Err(WorkRunControlError::Unconfirmed(message)) => {
                self.mode = WorkRunControlMode::Retry;
                self.feedback = Some(message.clone());
                false
            }
            Err(WorkRunControlError::Rejected(message)) => {
                self.mode = WorkRunControlMode::List;
                self.feedback = Some(message.clone());
                self.retry = None;
                false
            }
        }
    }

    pub fn close(&mut self) {
        if self.mode == WorkRunControlMode::Submitting {
            return;
        }
        self.mode = WorkRunControlMode::Closed;
        self.feedback = None;
        if self.retry.is_none() {
            self.selected = None;
            self.escalation_fence = None;
        }
    }
}

const fn previous_escalation_decision(decision: EscalationDecision) -> EscalationDecision {
    match decision {
        EscalationDecision::Resume | EscalationDecision::Cancel => EscalationDecision::Resume,
        EscalationDecision::Fail => EscalationDecision::Cancel,
    }
}

const fn next_escalation_decision(decision: EscalationDecision) -> EscalationDecision {
    match decision {
        EscalationDecision::Resume => EscalationDecision::Cancel,
        EscalationDecision::Cancel | EscalationDecision::Fail => EscalationDecision::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use usagi_core::domain::supervisor::{EscalationRecord, ExecutionPolicy};

    fn run(state: SupervisorRunState) -> SupervisorRunQuery {
        SupervisorRunQuery {
            supervisor_run_id: SupervisorRunId::new(),
            state_revision: 1,
            state,
            terminal_at: None,
            terminal_reason: None,
            display_label: Some("Control Goal".into()),
            policy: ExecutionPolicy::default(),
            escalation: None,
            tasks: Vec::new(),
            provenance: Vec::new(),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn escalated_run() -> SupervisorRunQuery {
        let mut escalated = run(SupervisorRunState::Escalated);
        escalated.escalation = Some(EscalationRecord {
            escalation_id: OperationId::new(),
            reason: "checks failed".into(),
            blocking_task_id: None,
            safe_evidence: "bounded".into(),
            choices: vec!["retry".into(), "cancel".into(), "fail".into()],
            created_at: now(),
        });
        escalated
    }

    #[test]
    fn cancel_confirmation_retries_the_same_operation() {
        let running = run(SupervisorRunState::Running);
        let runs = vec![running.clone()];
        let mut control = WorkRunControl::default();
        assert_eq!(
            control.handle(WorkRunControlAction::Toggle, &runs, true),
            WorkRunControlOutcome::Consumed
        );
        assert_eq!(control.mode(), WorkRunControlMode::List);
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::ConfirmCancel);
        let request = control
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("confirmed cancellation submits");
        assert_eq!(
            request.command,
            SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: running.supervisor_run_id,
                reason: "cancelled by local operator".into(),
            }
        );
        assert_eq!(request.observed_state_revision, running.state_revision);
        assert!(!control.complete(
            request.operation_id,
            &Err(WorkRunControlError::Unconfirmed(
                "outcome is unavailable".into()
            ))
        ));
        assert_eq!(control.mode(), WorkRunControlMode::Retry);
        let retry = control
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("retry submits");
        assert_eq!(retry, request);
        assert!(!control.complete(request.operation_id, &Ok(running.clone())));
        assert_eq!(control.mode(), WorkRunControlMode::Retry);
        let retry = control
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("a stale result keeps the same retry");
        assert_eq!(retry, request);

        let mut cancelled = running;
        cancelled.state_revision += 1;
        cancelled.state = SupervisorRunState::Cancelled;
        assert!(control.complete(request.operation_id, &Ok(cancelled)));
        assert_eq!(control.mode(), WorkRunControlMode::List);
        assert_eq!(control.feedback(), Some("Work Run updated"));

        let refusal = WorkRunControlError::Rejected("The Work Run already finished".into());
        assert_eq!(refusal.message(), "The Work Run already finished");
        let mut rejected = WorkRunControl::default();
        let _ = rejected.handle(WorkRunControlAction::Toggle, &runs, true);
        let _ = rejected.handle(WorkRunControlAction::Enter, &runs, true);
        let rejected_request = rejected
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("a fresh cancellation submits");
        assert!(!rejected.complete(rejected_request.operation_id, &Err(refusal)));
        assert_eq!(rejected.mode(), WorkRunControlMode::List);
        assert_eq!(rejected.feedback(), Some("The Work Run already finished"));
        rejected.close();
        let _ = rejected.handle(WorkRunControlAction::Toggle, &runs, true);
        assert_eq!(rejected.mode(), WorkRunControlMode::List);
    }

    #[test]
    fn escalation_uses_the_observed_fence_and_stale_data_is_read_only() {
        let escalated = escalated_run();
        let escalation_id = escalated.escalation.as_ref().unwrap().escalation_id;
        let runs = vec![escalated.clone()];
        let mut control = WorkRunControl::default();
        let _ = control.handle(WorkRunControlAction::Toggle, &runs, true);
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::ResolveEscalation);
        let _ = control.handle(WorkRunControlAction::Down, &runs, true);
        let request = control
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("decision submits");
        assert_eq!(
            request.command,
            SupervisorWorkspaceCommand::ResolveEscalation {
                supervisor_run_id: escalated.supervisor_run_id,
                escalation_id,
                decision: EscalationDecision::Cancel,
            }
        );

        let mut stale = WorkRunControl::default();
        let _ = stale.handle(WorkRunControlAction::Toggle, &runs, false);
        let _ = stale.handle(WorkRunControlAction::Enter, &runs, false);
        assert_eq!(stale.mode(), WorkRunControlMode::List);
        assert_eq!(
            stale.feedback(),
            Some("Refresh Work Runs before changing one")
        );

        let mut changed = WorkRunControl::default();
        let _ = changed.handle(WorkRunControlAction::Toggle, &runs, true);
        let _ = changed.handle(WorkRunControlAction::Enter, &runs, true);
        let mut replacement = escalated;
        replacement.state_revision += 1;
        replacement.escalation.as_mut().unwrap().escalation_id = OperationId::new();
        let outcome = changed.handle(WorkRunControlAction::Enter, &[replacement], true);
        assert_eq!(outcome, WorkRunControlOutcome::Consumed);
        assert_eq!(changed.mode(), WorkRunControlMode::List);
        assert_eq!(
            changed.feedback(),
            Some("The Work Run decision is no longer current")
        );
    }

    #[test]
    fn command_confirmation_rechecks_fresh_state_and_result_semantics() {
        let running = run(SupervisorRunState::Running);
        let runs = vec![running.clone()];
        let mut stale_cancel = WorkRunControl::default();
        let _ = stale_cancel.handle(WorkRunControlAction::Toggle, &runs, true);
        let _ = stale_cancel.handle(WorkRunControlAction::Enter, &runs, true);
        let _ = stale_cancel.handle(WorkRunControlAction::Enter, &runs, false);
        assert_eq!(stale_cancel.mode(), WorkRunControlMode::List);
        assert_eq!(
            stale_cancel.feedback(),
            Some("Refresh Work Runs before changing one")
        );

        let mut changed_cancel = WorkRunControl::default();
        let _ = changed_cancel.handle(WorkRunControlAction::Toggle, &runs, true);
        let _ = changed_cancel.handle(WorkRunControlAction::Enter, &runs, true);
        let mut finished = running.clone();
        finished.state = SupervisorRunState::Succeeded;
        let _ = changed_cancel.handle(WorkRunControlAction::Enter, &[finished], true);
        assert_eq!(changed_cancel.mode(), WorkRunControlMode::List);
        assert_eq!(
            changed_cancel.feedback(),
            Some("The Work Run action changed; review it again")
        );

        let escalated = escalated_run();
        let escalation_id = escalated.escalation.as_ref().unwrap().escalation_id;
        let mut stale_decision = WorkRunControl::default();
        let _ = stale_decision.handle(
            WorkRunControlAction::Toggle,
            std::slice::from_ref(&escalated),
            true,
        );
        let _ = stale_decision.handle(
            WorkRunControlAction::Enter,
            std::slice::from_ref(&escalated),
            true,
        );
        let _ = stale_decision.handle(
            WorkRunControlAction::Enter,
            std::slice::from_ref(&escalated),
            false,
        );
        assert_eq!(stale_decision.mode(), WorkRunControlMode::List);
        assert_eq!(
            stale_decision.feedback(),
            Some("Refresh Work Runs before changing one")
        );

        for (decision, state) in [
            (EscalationDecision::Resume, SupervisorRunState::Running),
            (EscalationDecision::Cancel, SupervisorRunState::Cancelled),
            (EscalationDecision::Fail, SupervisorRunState::Failed),
        ] {
            let request = WorkRunControlRequest {
                operation_id: OperationId::new(),
                command: SupervisorWorkspaceCommand::ResolveEscalation {
                    supervisor_run_id: escalated.supervisor_run_id,
                    escalation_id,
                    decision,
                },
                observed_state_revision: escalated.state_revision,
            };
            let mut result = escalated.clone();
            result.state_revision += 1;
            result.state = state;
            result.escalation = None;
            assert!(request.accepts_result(&result));
            result.escalation = escalated.escalation.clone();
            assert!(!request.accepts_result(&result));
        }
    }

    #[test]
    fn list_navigation_and_confirmation_are_total_and_stable_by_id() {
        let first = run(SupervisorRunState::Running);
        let finished = run(SupervisorRunState::Succeeded);
        let undecidable = run(SupervisorRunState::Escalated);
        let runs = vec![first.clone(), finished.clone(), undecidable.clone()];
        let mut control = WorkRunControl::default();

        assert_eq!(
            control.handle(WorkRunControlAction::Escape, &runs, true),
            WorkRunControlOutcome::Consumed
        );
        assert_eq!(WorkRunControlOutcome::Consumed.into_request(), None);
        let _ = control.handle(WorkRunControlAction::Toggle, &runs, true);
        assert_eq!(control.selected(), Some(first.supervisor_run_id));
        control.sync_selection(&runs);
        assert_eq!(control.selected(), Some(first.supervisor_run_id));

        let _ = control.handle(WorkRunControlAction::Down, &runs, true);
        assert_eq!(control.selected(), Some(finished.supervisor_run_id));
        let _ = control.handle(WorkRunControlAction::NextDecision, &runs, true);
        assert_eq!(control.selected(), Some(undecidable.supervisor_run_id));
        let _ = control.handle(WorkRunControlAction::Down, &runs, true);
        assert_eq!(control.selected(), Some(undecidable.supervisor_run_id));
        let _ = control.handle(WorkRunControlAction::PreviousDecision, &runs, true);
        assert_eq!(control.selected(), Some(finished.supervisor_run_id));
        let _ = control.handle(WorkRunControlAction::Up, &runs, true);
        assert_eq!(control.selected(), Some(first.supervisor_run_id));

        control.selected = Some(SupervisorRunId::new());
        let _ = control.handle(WorkRunControlAction::Down, &runs, true);
        assert_eq!(control.selected(), Some(first.supervisor_run_id));
        control.selected = None;
        let _ = control.handle(WorkRunControlAction::Up, &runs, true);
        assert_eq!(control.selected(), Some(first.supervisor_run_id));

        control.selected = Some(finished.supervisor_run_id);
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(
            control.feedback(),
            Some("This Work Run is already finished")
        );
        control.selected = Some(undecidable.supervisor_run_id);
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(
            control.feedback(),
            Some("This Work Run has no current decision")
        );
        control.selected = None;
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(control.feedback(), Some("No Work Run is selected"));

        control.selected = Some(first.supervisor_run_id);
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::ConfirmCancel);
        assert_eq!(
            control.handle(WorkRunControlAction::Toggle, &runs, true),
            WorkRunControlOutcome::Consumed
        );
        let replacement = run(SupervisorRunState::Running);
        control.sync_selection(std::slice::from_ref(&replacement));
        assert_eq!(control.selected(), Some(first.supervisor_run_id));
        assert_eq!(
            control.handle(
                WorkRunControlAction::Enter,
                std::slice::from_ref(&replacement),
                true,
            ),
            WorkRunControlOutcome::Consumed
        );
        assert_eq!(control.mode(), WorkRunControlMode::List);
        assert_eq!(
            control.feedback(),
            Some("The selected Work Run is no longer available")
        );
        control.sync_selection(&runs);
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        let _ = control.handle(WorkRunControlAction::Escape, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::List);
        let _ = control.handle(WorkRunControlAction::Toggle, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::Closed);

        control.mode = WorkRunControlMode::ConfirmCancel;
        control.selected = None;
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::List);
        assert_eq!(
            control.feedback(),
            Some("The selected Work Run is no longer available")
        );

        let _ = control.handle(WorkRunControlAction::Toggle, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::Closed);
        assert_eq!(control.selected(), None);
        control.sync_selection(&[]);
        assert_eq!(control.selected(), None);
    }

    #[test]
    fn decision_submission_completion_and_retry_paths_fail_closed() {
        let escalated = escalated_run();
        let runs = vec![escalated.clone()];
        let mut control = WorkRunControl::default();
        let _ = control.handle(WorkRunControlAction::Toggle, &runs, true);
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);

        let _ = control.handle(WorkRunControlAction::Up, &runs, true);
        assert_eq!(control.decision(), EscalationDecision::Resume);
        let _ = control.handle(WorkRunControlAction::Down, &runs, true);
        assert_eq!(control.decision(), EscalationDecision::Cancel);
        let _ = control.handle(WorkRunControlAction::NextDecision, &runs, true);
        assert_eq!(control.decision(), EscalationDecision::Fail);
        let _ = control.handle(WorkRunControlAction::Down, &runs, true);
        assert_eq!(control.decision(), EscalationDecision::Fail);
        let _ = control.handle(WorkRunControlAction::PreviousDecision, &runs, true);
        assert_eq!(control.decision(), EscalationDecision::Cancel);
        let _ = control.handle(WorkRunControlAction::Up, &runs, true);
        assert_eq!(control.decision(), EscalationDecision::Resume);
        assert_eq!(
            control.handle(WorkRunControlAction::Toggle, &runs, true),
            WorkRunControlOutcome::Consumed
        );
        let _ = control.handle(WorkRunControlAction::Escape, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::List);

        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        let request = control
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("decision submits");
        assert_eq!(
            control.handle(WorkRunControlAction::Escape, &runs, true),
            WorkRunControlOutcome::Consumed
        );
        assert!(!control.complete(OperationId::new(), &Ok(escalated.clone())));
        assert_eq!(control.mode(), WorkRunControlMode::Submitting);
        assert!(!control.complete(
            request.operation_id,
            &Ok(run(SupervisorRunState::Cancelled)),
        ));
        assert_eq!(control.mode(), WorkRunControlMode::Retry);
        assert_eq!(
            control.feedback(),
            Some("daemon returned an invalid Work Run result")
        );
        let retry = control
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("invalid result preserves the operation for retry");
        assert_eq!(retry, request);
        control.close();
        assert_eq!(control.mode(), WorkRunControlMode::Submitting);
        assert_eq!(control.selected(), Some(escalated.supervisor_run_id));
        assert!(!control.complete(
            request.operation_id,
            &Err(WorkRunControlError::Unconfirmed(
                "outcome unavailable".into()
            ))
        ));
        assert_eq!(control.mode(), WorkRunControlMode::Retry);
        assert_eq!(control.feedback(), Some("outcome unavailable"));
        control.close();
        assert_eq!(control.mode(), WorkRunControlMode::Closed);
        let _ = control.handle(WorkRunControlAction::Toggle, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::Retry);
        let reopened = control
            .handle(WorkRunControlAction::Enter, &runs, true)
            .into_request()
            .expect("reopening preserves the unconfirmed operation");
        assert_eq!(reopened, request);
        assert!(!control.complete(
            request.operation_id,
            &Err(WorkRunControlError::Unconfirmed(
                "outcome unavailable".into()
            ))
        ));
        assert_eq!(
            control.handle(WorkRunControlAction::PreviousDecision, &runs, true),
            WorkRunControlOutcome::Consumed
        );
        let _ = control.handle(WorkRunControlAction::Escape, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::Closed);

        control.mode = WorkRunControlMode::Retry;
        control.retry = None;
        let _ = control.handle(WorkRunControlAction::Enter, &runs, true);
        assert_eq!(control.mode(), WorkRunControlMode::List);
        assert_eq!(
            control.feedback(),
            Some("No Work Run action is available to retry")
        );

        control.mode = WorkRunControlMode::ResolveEscalation;
        control.selected = Some(escalated.supervisor_run_id);
        let _ = control.handle(WorkRunControlAction::Enter, &[], true);
        assert_eq!(control.mode(), WorkRunControlMode::List);
        assert_eq!(
            control.feedback(),
            Some("The Work Run decision is no longer current")
        );
    }
}

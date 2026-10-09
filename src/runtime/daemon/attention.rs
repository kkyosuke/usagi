//! Read-only summary of already adopted workspaces. Never adopts or reconciles.
use super::{
    ConnectionWorkspace, SharedAgentRuntime, SharedPrInventory, SharedSupervisorRuntime, envelope,
};
use serde::Deserialize;
use usagi_core::domain::{
    agent::AgentStatus,
    attention::{AttentionItem, AttentionKind, WorkspaceAttention},
    id::{SessionId, WorkspaceId},
    pr_inventory::{PrChecksState, PrEntry, PrRefreshState, PrReviewDecision, PrState},
    presentation_text::sanitize_presentation_line,
    session_lifecycle::SessionLifecycle,
    workflow::{Phase, WorkflowSnapshot},
};
use usagi_core::infrastructure::{
    ipc::{Envelope, ErrorCode, ProtocolError, RequestId, ResponseOutcome, ServerHello},
    store::user_decision::UserDecisionStore,
};

#[derive(Deserialize)]
struct SessionRow {
    session_id: SessionId,
    name: String,
    lifecycle: SessionLifecycle,
}

#[allow(clippy::too_many_arguments)] // The existing IPC owners are borrowed, not copied into another authority.
pub(super) fn dispatch(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    decisions: &UserDecisionStore,
    prs: &SharedPrInventory,
    supervisor: &SharedSupervisorRuntime,
    workspace: WorkspaceId,
    request_id: RequestId,
    raw: &serde_json::Value,
    hello: &ServerHello,
) -> Envelope {
    let result = if raw.get("caller_context").is_some() || raw.get("_caller_credential").is_some() {
        Err(ProtocolError::new(
            ErrorCode::PermissionDenied,
            "Attention is human-only",
        ))
    } else {
        snapshot(agent, bound, decisions, prs, supervisor, workspace)
            .and_then(|snapshot| serde_json::to_value(snapshot).map_err(unavailable))
    };
    match result {
        Ok(body) => envelope(hello, request_id, ResponseOutcome::Ok, body),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

fn unavailable<T>(_: T) -> ProtocolError {
    ProtocolError::new(ErrorCode::Unavailable, "Workspace attention is unavailable")
}

fn snapshot(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    decisions: &UserDecisionStore,
    prs: &SharedPrInventory,
    supervisor: &SharedSupervisorRuntime,
    workspace: WorkspaceId,
) -> Result<WorkspaceAttention, ProtocolError> {
    let tenant = bound.workspaces.workspace(workspace).ok_or_else(|| {
        ProtocolError::new(ErrorCode::OwnershipUnknown, "Workspace is not adopted")
    })?;
    let value = tenant
        .runtime()
        .lock()
        .map_err(unavailable)?
        .snapshot()
        .map_err(unavailable)?;
    let sessions: Vec<SessionRow> =
        serde_json::from_value(value["sessions"].clone()).map_err(unavailable)?;
    let mut items = Vec::new();
    for run in supervisor
        .lock()
        .map_err(unavailable)?
        .list_workspace(workspace)
        .map_err(unavailable)?
    {
        if let Some(item) = work_run_attention(&run) {
            items.push(item);
        }
    }
    for decision in decisions.pending(workspace).map_err(unavailable)? {
        let label = match decision.owner.session_id {
            Some(id) => sessions
                .iter()
                .find(|session| session.session_id == id)
                .map(|session| session.name.as_str()),
            None => Some("Director"),
        };
        // A removed incarnation must not turn into a visit to a same-name session.
        if let Some(label) = label {
            items.push(AttentionItem {
                key: format!("decision:{}", decision.decision_id),
                session: decision.owner.session_id,
                label: safe(label),
                kind: AttentionKind::Decision,
                reason: safe(&decision.title),
            });
        }
    }
    let agent = agent.lock().map_err(unavailable)?;
    let observation = agent.workspace_observation(workspace)?;
    let mut workflows = Vec::new();
    for session in &sessions {
        workflows.push(
            usagi_daemon::usecase::workflow::projection(
                agent.dispatch_store(),
                workspace,
                session.session_id,
            )
            .map_err(unavailable)?,
        );
    }
    drop(agent);
    let ids = sessions
        .iter()
        .map(|session| session.session_id)
        .collect::<Vec<_>>();
    let prs = prs
        .lock()
        .map_err(unavailable)?
        .snapshots(&ids)
        .map_err(unavailable)?;
    for ((session, workflow), prs) in sessions.iter().zip(&workflows).zip(&prs) {
        let pending_decision = items
            .iter()
            .any(|item| item.session == Some(session.session_id));
        if let Some((kind, reason)) = session_attention(
            session,
            workflow,
            observation
                .session_statuses
                .get(&session.session_id)
                .copied(),
        ) && (!pending_decision
            || !matches!(kind, AttentionKind::Blocked | AttentionKind::Running))
        {
            items.push(session_item(session, kind, &reason));
        }
        for pr in &prs.entries {
            if let Some((kind, reason)) = pr_attention(pr) {
                // Ready workflow and its PR are one review action.
                if workflow.run.as_ref().is_some_and(|run| {
                    run.phase == Phase::Ready && run.pr_url.as_deref() == Some(pr.identity.as_url())
                }) {
                    continue;
                }
                let mut item = session_item(session, kind, reason);
                item.key = format!("pr:{}:{}", session.session_id, pr.identity.as_url());
                item.reason = format!("PR #{} · {}", pr.identity.number(), reason);
                items.push(item);
            }
        }
    }
    // Fail the whole observation rather than silently omitting waiting work.
    if items.len() > 1024 {
        return Err(unavailable("attention item limit"));
    }
    items.sort_by(|a, b| (a.kind, &a.label, &a.key).cmp(&(b.kind, &b.label, &b.key)));
    Ok(WorkspaceAttention { workspace, items })
}

fn safe(text: &str) -> String {
    sanitize_presentation_line(text).chars().take(160).collect()
}

fn session_item(session: &SessionRow, kind: AttentionKind, reason: &str) -> AttentionItem {
    AttentionItem {
        key: format!("session:{}", session.session_id),
        session: Some(session.session_id),
        label: safe(&session.name),
        kind,
        reason: safe(reason),
    }
}

fn session_attention(
    session: &SessionRow,
    workflow: &WorkflowSnapshot,
    status: Option<AgentStatus>,
) -> Option<(AttentionKind, String)> {
    if session.lifecycle == SessionLifecycle::Failed {
        return Some((AttentionKind::Blocked, "Session lifecycle failed".into()));
    }
    if session.lifecycle != SessionLifecycle::Available {
        return Some((
            AttentionKind::System,
            format!("Session {:?}", session.lifecycle),
        ));
    }
    if let Some(pending) = &workflow.pending_start {
        return Some(pending.error.as_ref().map_or_else(
            || (AttentionKind::System, "Workflow starting".into()),
            |error| (AttentionKind::Blocked, error.clone()),
        ));
    }
    if let Some(run) = &workflow.run {
        let kind = match run.phase {
            Phase::Ready => AttentionKind::Review,
            Phase::Waiting => AttentionKind::Blocked,
            Phase::Verifying => AttentionKind::System,
            _ => AttentionKind::Running,
        };
        return Some((
            kind,
            run.waiting_reason
                .clone()
                .unwrap_or_else(|| run.phase.label().into()),
        ));
    }
    match status {
        Some(AgentStatus::Failed) => Some((AttentionKind::Blocked, "Agent failed".into())),
        Some(AgentStatus::Running | AgentStatus::Starting) => {
            Some((AttentionKind::Running, "Agent working".into()))
        }
        // Idle/exited alone cannot establish that a person needs to act.
        _ => None,
    }
}

fn pr_attention(pr: &PrEntry) -> Option<(AttentionKind, &'static str)> {
    if pr.state != PrState::Open {
        return None;
    }
    if pr.refresh != PrRefreshState::Idle || pr.head_oid.is_none() {
        return Some((AttentionKind::System, "PR status unknown / refreshing"));
    }
    if pr.checks == Some(PrChecksState::Failing)
        || pr.review == Some(PrReviewDecision::ChangesRequested)
    {
        return Some((AttentionKind::Blocked, "CI failed / changes requested"));
    }
    if pr.checks == Some(PrChecksState::Pending) {
        return Some((AttentionKind::System, "CI pending"));
    }
    if !pr.draft {
        return Some((AttentionKind::Review, "Review / merge decision"));
    }
    None
}

fn work_run_attention(
    run: &usagi_core::domain::supervisor::SupervisorRunQuery,
) -> Option<AttentionItem> {
    use usagi_core::domain::supervisor::SupervisorRunState;
    let (kind, reason) = match run.state {
        SupervisorRunState::Escalated | SupervisorRunState::WaitingForDecision => (
            AttentionKind::Decision,
            run.escalation
                .as_ref()
                .map_or("Work Run needs a decision", |escalation| {
                    escalation.reason.as_str()
                }),
        ),
        SupervisorRunState::Failed => (
            AttentionKind::Blocked,
            run.terminal_reason.as_deref().unwrap_or("Work Run failed"),
        ),
        SupervisorRunState::Verifying => (AttentionKind::System, "Work Run verifying artifacts"),
        SupervisorRunState::Planning | SupervisorRunState::Running => {
            (AttentionKind::Running, "Work Run active")
        }
        SupervisorRunState::Succeeded | SupervisorRunState::Cancelled => return None,
    };
    Some(AttentionItem {
        key: format!("run:{}", run.supervisor_run_id),
        session: None,
        label: safe(run.display_label.as_deref().unwrap_or("Work Run")),
        kind,
        reason: safe(reason),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::{
        id::{AgentId, OperationId},
        workflow::{WorkflowPendingStart, WorkflowRun},
    };
    fn session() -> SessionRow {
        SessionRow {
            session_id: SessionId::new(),
            name: "build".into(),
            lifecycle: SessionLifecycle::Available,
        }
    }
    fn workflow(session: SessionId) -> WorkflowSnapshot {
        WorkflowSnapshot {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            revision_limit: 3,
            session,
            run: None,
            pending_start: None,
            finished: vec![],
        }
    }

    #[test]
    fn attention_classification_does_not_turn_idle_or_agent_review_into_human_wait() {
        let mut session = session();
        let mut workflow = workflow(session.session_id);
        for status in [None, Some(AgentStatus::Idle), Some(AgentStatus::Exited)] {
            assert!(session_attention(&session, &workflow, status).is_none());
        }
        for status in [AgentStatus::Running, AgentStatus::Starting] {
            assert_eq!(
                session_attention(&session, &workflow, Some(status))
                    .unwrap()
                    .0,
                AttentionKind::Running
            );
        }
        assert_eq!(
            session_attention(&session, &workflow, Some(AgentStatus::Failed))
                .unwrap()
                .0,
            AttentionKind::Blocked
        );
        session.lifecycle = SessionLifecycle::Failed;
        assert_eq!(
            session_attention(&session, &workflow, None).unwrap().0,
            AttentionKind::Blocked
        );
        session.lifecycle = SessionLifecycle::Creating;
        assert_eq!(
            session_attention(&session, &workflow, None).unwrap().0,
            AttentionKind::System
        );
        session.lifecycle = SessionLifecycle::Available;
        workflow.pending_start = Some(WorkflowPendingStart {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            revision_limit: 3,
            operation_id: OperationId::new(),
            goal: "build".into(),
            error: None,
            issue: None,
        });
        assert_eq!(
            session_attention(&session, &workflow, None).unwrap().0,
            AttentionKind::System
        );
        workflow.pending_start.as_mut().unwrap().error = Some("credentials unavailable".into());
        assert_eq!(
            session_attention(&session, &workflow, None).unwrap(),
            (AttentionKind::Blocked, "credentials unavailable".into())
        );
        workflow.pending_start = None;
        workflow.run = Some(WorkflowRun {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            id: OperationId::new(),
            session: session.session_id,
            goal: "build".into(),
            implementer: AgentId::new(),
            reviewer: None,
            phase: Phase::Reviewing,
            revision_limit: 3,
            revisions: 0,
            review: None,
            waiting_reason: None,
            pr_url: None,
            issue: None,
            instructions: vec![],
            history: vec![],
        });
        for (phase, expected) in [
            (Phase::Reviewing, AttentionKind::Running),
            (Phase::Ready, AttentionKind::Review),
            (Phase::Waiting, AttentionKind::Blocked),
            (Phase::Verifying, AttentionKind::System),
        ] {
            workflow.run.as_mut().unwrap().phase = phase;
            assert_eq!(
                session_attention(&session, &workflow, None).unwrap().0,
                expected
            );
        }
        workflow.run.as_mut().unwrap().waiting_reason = Some("specific reason".into());
        assert_eq!(
            session_attention(&session, &workflow, None).unwrap().1,
            "specific reason"
        );
        let item = session_item(&session, AttentionKind::Decision, "hello\u{1b}\nworld");
        assert!(!item.reason.contains('\u{1b}'));
        assert_eq!(safe(&"x".repeat(200)).len(), 160);
    }

    #[test]
    fn attention_pr_classification_ignores_closed_and_distinguishes_ci_and_drafts() {
        let mut pr = PrEntry::new(
            usagi_core::domain::pr_inventory::canonicalize(
                "https://github.com/example/repo/pull/1",
            )
            .unwrap(),
        );
        assert_eq!(pr_attention(&pr).unwrap().0, AttentionKind::System);
        pr.head_oid = Some("a".repeat(40));
        pr.refresh = PrRefreshState::Idle;
        assert_eq!(pr_attention(&pr).unwrap().0, AttentionKind::Review);
        pr.draft = true;
        assert!(pr_attention(&pr).is_none());
        pr.checks = Some(PrChecksState::Pending);
        assert_eq!(pr_attention(&pr).unwrap().0, AttentionKind::System);
        pr.checks = Some(PrChecksState::Failing);
        assert_eq!(pr_attention(&pr).unwrap().0, AttentionKind::Blocked);
        pr.checks = Some(PrChecksState::Passing);
        pr.review = Some(PrReviewDecision::ChangesRequested);
        assert_eq!(pr_attention(&pr).unwrap().0, AttentionKind::Blocked);
        for state in [PrState::Merged, PrState::Closed, PrState::Dismissed] {
            pr.state = state;
            assert!(pr_attention(&pr).is_none());
        }
        assert_eq!(unavailable("private error").code, ErrorCode::Unavailable);
    }

    #[test]
    fn attention_work_runs_surface_escalations_but_not_completed_history() {
        use usagi_core::domain::supervisor::{
            EscalationRecord, SupervisorRunId, SupervisorRunQuery, SupervisorRunState,
        };
        let mut run = SupervisorRunQuery {
            supervisor_run_id: SupervisorRunId::new(),
            state_revision: 1,
            state: SupervisorRunState::Planning,
            terminal_at: None,
            terminal_reason: None,
            display_label: None,
            root_agent_id: None,
            policy: usagi_core::domain::supervisor::ExecutionPolicy::default(),
            escalation: None,
            tasks: vec![],
            provenance: vec![],
        };
        for (state, expected) in [
            (SupervisorRunState::Planning, AttentionKind::Running),
            (SupervisorRunState::Verifying, AttentionKind::System),
            (SupervisorRunState::Failed, AttentionKind::Blocked),
            (
                SupervisorRunState::WaitingForDecision,
                AttentionKind::Decision,
            ),
        ] {
            run.state = state;
            assert_eq!(work_run_attention(&run).unwrap().kind, expected);
        }
        run.state = SupervisorRunState::Escalated;
        run.escalation = Some(EscalationRecord {
            escalation_id: OperationId::new(),
            reason: "retry budget".into(),
            blocking_task_id: None,
            safe_evidence: String::new(),
            choices: vec![],
            created_at: chrono::Utc::now(),
        });
        assert_eq!(work_run_attention(&run).unwrap().reason, "retry budget");
        run.state = SupervisorRunState::Succeeded;
        assert!(work_run_attention(&run).is_none());
        run.state = SupervisorRunState::Cancelled;
        assert!(work_run_attention(&run).is_none());
    }
}

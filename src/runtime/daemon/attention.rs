//! Read-only summary of already adopted workspaces. Never adopts or reconciles.
use super::{ConnectionWorkspace, SharedAgentRuntime, envelope};
use serde::Deserialize;
use usagi_core::domain::{
    agent::AgentStatus,
    attention::{
        ATTENTION_ITEMS_MAX, AttentionItem, AttentionKind, AttentionTarget, WorkspaceAttention,
    },
    id::{SessionId, WorkspaceId},
    presentation_text::sanitize_presentation_line,
    session_lifecycle::{AgentPhase, SessionLifecycle},
    user_decision::UserDecision,
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

pub(super) fn dispatch(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    decisions: &UserDecisionStore,
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
        snapshot(agent, bound, decisions, workspace)
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
    let decisions = decisions.pending(workspace).map_err(unavailable)?;
    let agent = agent.lock().map_err(unavailable)?;
    let observation = agent.workspace_observation(workspace)?;
    let phases = sessions
        .iter()
        .map(|session| agent.session_phase(session.session_id))
        .collect::<Vec<_>>();
    drop(agent);
    project(
        workspace,
        &sessions,
        &decisions,
        &observation.session_statuses,
        &phases,
    )
}

fn project(
    workspace: WorkspaceId,
    sessions: &[SessionRow],
    decisions: &[UserDecision],
    statuses: &std::collections::BTreeMap<SessionId, AgentStatus>,
    phases: &[AgentPhase],
) -> Result<WorkspaceAttention, ProtocolError> {
    let mut items = Vec::new();
    for decision in decisions {
        let label = match decision.owner.session_id {
            Some(id) => sessions
                .iter()
                .find(|session| session.session_id == id)
                .map(|session| session.name.as_str()),
            None => Some("Workspace"),
        };
        // A removed incarnation must not turn into a visit to a same-name session.
        if let Some(label) = label {
            items.push(AttentionItem {
                key: format!("decision:{}", decision.decision_id),
                session: decision.owner.session_id,
                label: safe(label),
                kind: AttentionKind::Decision,
                reason: safe(&decision.title),
                target: Some(AttentionTarget::Decision {
                    decision_id: decision.decision_id,
                }),
                status_unknown: false,
            });
        }
    }
    for (session, phase) in sessions.iter().zip(phases) {
        let pending_decision = items
            .iter()
            .any(|item| item.session == Some(session.session_id));
        if let Some((kind, reason)) =
            session_attention(session, *phase, statuses.get(&session.session_id).copied())
            && !(pending_decision
                && session.lifecycle == SessionLifecycle::Available
                && *phase == AgentPhase::Waiting
                && kind == AttentionKind::System)
        {
            items.push(session_item(session, kind, &reason));
        }
    }
    // Fail the whole observation rather than silently omitting waiting work.
    if items.len() > ATTENTION_ITEMS_MAX {
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
        target: None,
        status_unknown: false,
    }
}

fn session_attention(
    session: &SessionRow,
    phase: AgentPhase,
    status: Option<AgentStatus>,
) -> Option<(AttentionKind, String)> {
    if session.lifecycle == SessionLifecycle::Failed {
        return Some((AttentionKind::Blocked, "Session failed".into()));
    }
    if session.lifecycle != SessionLifecycle::Available {
        return Some((
            AttentionKind::System,
            format!("Session {:?}", session.lifecycle),
        ));
    }
    if phase == AgentPhase::Waiting
        && !matches!(
            status,
            Some(AgentStatus::Running | AgentStatus::Starting | AgentStatus::Failed)
        )
    {
        return Some((AttentionKind::System, "Agent is waiting".into()));
    }
    if phase == AgentPhase::Interrupted {
        return Some((AttentionKind::Blocked, "Agent interrupted".into()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::{
        agent::CallerRef,
        id::{AgentId, OperationId, UserDecisionId},
        user_decision::{UserDecisionOwner, UserDecisionSelectionMode, UserDecisionStatus},
    };

    fn session() -> SessionRow {
        SessionRow {
            session_id: SessionId::new(),
            name: "build".into(),
            lifecycle: SessionLifecycle::Available,
        }
    }
    fn decision(workspace: WorkspaceId, session: Option<SessionId>) -> UserDecision {
        UserDecision {
            decision_id: UserDecisionId::new(),
            owner: UserDecisionOwner {
                workspace_id: workspace,
                session_id: session,
                caller: CallerRef {
                    session_id: session,
                    agent_id: AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "Choose\u{1b}\nnext step".into(),
            prompt: "Choose".into(),
            options: vec![],
            allow_freeform: true,
            allow_comment: false,
            require_confirmation: false,
            recommendation: None,
            selection_limits: None,
            selection_mode: UserDecisionSelectionMode::Single,
            context: vec![],
            expires_at: None,
            idempotency_key: None,
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: chrono::Utc::now(),
            resolved_at: None,
        }
    }
    #[test]
    fn session_attention_distinguishes_activity_waiting_interruption_and_intentional_sleep() {
        let mut row = session();
        for phase in [
            AgentPhase::Absent,
            AgentPhase::Ready,
            AgentPhase::Sleeping,
            AgentPhase::Ended,
            AgentPhase::Exited,
        ] {
            for status in [None, Some(AgentStatus::Idle), Some(AgentStatus::Exited)] {
                assert!(session_attention(&row, phase, status).is_none());
            }
        }
        for status in [AgentStatus::Starting, AgentStatus::Running] {
            assert_eq!(
                session_attention(&row, AgentPhase::Waiting, Some(status))
                    .unwrap()
                    .0,
                AttentionKind::Running
            );
        }
        assert_eq!(
            session_attention(&row, AgentPhase::Waiting, None)
                .unwrap()
                .0,
            AttentionKind::System
        );
        assert_eq!(
            session_attention(&row, AgentPhase::Interrupted, None)
                .unwrap()
                .0,
            AttentionKind::Blocked
        );
        assert_eq!(
            session_attention(&row, AgentPhase::Absent, Some(AgentStatus::Failed))
                .unwrap()
                .0,
            AttentionKind::Blocked
        );
        row.lifecycle = SessionLifecycle::Failed;
        assert_eq!(
            session_attention(&row, AgentPhase::Absent, None).unwrap().0,
            AttentionKind::Blocked
        );
        row.lifecycle = SessionLifecycle::Creating;
        assert_eq!(
            session_attention(&row, AgentPhase::Absent, None).unwrap().0,
            AttentionKind::System
        );
        assert_eq!(safe(&"x".repeat(200)).len(), 160);
        assert_eq!(unavailable("private error").code, ErrorCode::Unavailable);
    }

    #[test]
    fn projection_fences_decisions_deduplicates_wait_and_keeps_independent_failures() {
        let workspace = WorkspaceId::new();
        let mut sessions = vec![session(), session()];
        let decisions = vec![
            decision(workspace, Some(sessions[0].session_id)),
            decision(workspace, None),
            decision(workspace, Some(SessionId::new())),
        ];
        let phases = [AgentPhase::Waiting, AgentPhase::Running];
        let statuses = [(sessions[1].session_id, AgentStatus::Running)].into();
        let result = project(workspace, &sessions, &decisions, &statuses, &phases).unwrap();
        assert_eq!(result.items.len(), 3);
        assert_eq!(
            result
                .items
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            vec![
                AttentionKind::Decision,
                AttentionKind::Decision,
                AttentionKind::Running
            ]
        );
        assert!(
            result
                .items
                .iter()
                .all(|item| !item.reason.contains('\u{1b}') && !item.reason.contains('\n'))
        );
        assert!(result.items.iter().any(|item| item.target
            == Some(AttentionTarget::Decision {
                decision_id: decisions[0].decision_id
            })));
        assert!(result.items.iter().all(|item| !item.status_unknown));
        let failed_statuses = [(sessions[0].session_id, AgentStatus::Failed)].into();
        assert!(
            project(workspace, &sessions, &decisions, &failed_statuses, &phases)
                .unwrap()
                .items
                .iter()
                .any(|item| item.kind == AttentionKind::Blocked)
        );
        sessions[0].lifecycle = SessionLifecycle::Failed;
        let result = project(workspace, &sessions, &decisions, &statuses, &phases).unwrap();
        assert!(
            result
                .items
                .iter()
                .any(|item| item.kind == AttentionKind::Blocked)
        );
    }

    #[test]
    fn projection_rejects_oversized_backlogs_and_omits_idle_sessions() {
        let workspace = WorkspaceId::new();
        let sessions = [session()];
        let too_many = (0..=ATTENTION_ITEMS_MAX)
            .map(|_| decision(workspace, None))
            .collect::<Vec<_>>();
        assert_eq!(
            project(
                workspace,
                &[],
                &too_many,
                &std::collections::BTreeMap::default(),
                &[]
            )
            .unwrap_err()
            .code,
            ErrorCode::Unavailable
        );
        assert!(
            project(
                workspace,
                &sessions[..1],
                &[],
                &std::collections::BTreeMap::default(),
                &[AgentPhase::Absent]
            )
            .unwrap()
            .items
            .is_empty()
        );
    }
}

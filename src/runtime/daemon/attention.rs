//! Read-only summary of already adopted workspaces. Never adopts or reconciles.
use super::{ConnectionWorkspace, SharedAgentRuntime, envelope};
use usagi_core::domain::{
    agent::AgentStatus,
    attention::{
        ATTENTION_ITEMS_MAX, AttentionItem, AttentionKind, AttentionTarget, PrimaryResult,
        WorkspaceAttention,
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
        snapshot(
            agent,
            bound,
            decisions,
            workspace,
            raw.get("include_results")
                .and_then(serde_json::Value::as_bool)
                == Some(true),
        )
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
    include_results: bool,
) -> Result<WorkspaceAttention, ProtocolError> {
    let tenant = bound.workspaces.workspace(workspace).ok_or_else(|| {
        ProtocolError::new(ErrorCode::OwnershipUnknown, "Workspace is not adopted")
    })?;
    let runtime = tenant.runtime().lock().map_err(unavailable)?;
    let root = runtime.repository_root().to_owned();
    let sessions = runtime
        .session_lifecycles()
        .map_err(unavailable)?
        .into_iter()
        .map(|session| SessionRow {
            session_id: session.session_id,
            name: session.name,
            lifecycle: session.lifecycle,
        })
        .collect::<Vec<_>>();
    drop(runtime);
    let decisions = decisions.pending(workspace).map_err(unavailable)?;
    let agent = agent.lock().map_err(unavailable)?;
    let observations = agent.session_observations(workspace)?;
    let statuses = observations
        .iter()
        .filter_map(|(session, observation)| observation.status.map(|status| (*session, status)))
        .collect();
    let phases = sessions
        .iter()
        .map(|session| {
            observations
                .get(&session.session_id)
                .map_or(AgentPhase::Absent, |observation| observation.phase)
        })
        .collect::<Vec<_>>();
    let store = agent.dispatch_store().clone();
    drop(agent);
    let mut snapshot = project(workspace, &sessions, &decisions, &statuses, &phases)?;
    if !include_results {
        return Ok(snapshot);
    }
    add_results(&mut snapshot, &root, &sessions, &store)?;
    Ok(snapshot)
}

fn add_results(
    snapshot: &mut WorkspaceAttention,
    root: &std::path::Path,
    sessions: &[SessionRow],
    store: &usagi_core::infrastructure::store::dispatch::DispatchStore,
) -> Result<(), ProtocolError> {
    let names = sessions
        .iter()
        .map(|s| (s.session_id, &s.name))
        .collect::<std::collections::BTreeMap<_, _>>();
    for (session, run, primary) in store
        .session_results(&names.keys().copied().collect())
        .map_err(unavailable)?
    {
        if snapshot.items.len() >= ATTENTION_ITEMS_MAX {
            break;
        }
        let reason = match &primary {
            PrimaryResult::Artifact(path) => path.clone(),
            PrimaryResult::PullRequest(pr) => pr.as_url().to_owned(),
        };
        snapshot.items.push(AttentionItem {
            key: format!("result:{run}"),
            session: Some(session),
            label: safe(names[&session]),
            kind: AttentionKind::System,
            reason: safe(&reason),
            target: Some(AttentionTarget::Result { primary }),
            status_unknown: false,
        });
    }
    let published = snapshot
        .items
        .iter()
        .filter_map(|item| match &item.target {
            Some(AttentionTarget::Result {
                primary: PrimaryResult::Artifact(path),
            }) => Some((item.session, path.clone())),
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    for entry in sessions
        .iter()
        .filter(|s| s.lifecycle == SessionLifecycle::Available)
    {
        let remaining = ATTENTION_ITEMS_MAX - snapshot.items.len();
        if remaining == 0 {
            break;
        }
        // A malformed manifest in one session cannot hide another session's questions.
        let Ok(paths) = usagi_core::infrastructure::artifacts::indexed_session_files(
            root,
            &entry.name,
            remaining,
        ) else {
            continue;
        };
        for path in paths.into_iter().filter(|path| {
            PrimaryResult::valid_artifact(path)
                && !published.contains(&(Some(entry.session_id), path.clone()))
        }) {
            snapshot.items.push(AttentionItem {
                key: format!("artifact:{}:{path}", entry.session_id),
                session: Some(entry.session_id),
                label: safe(&entry.name),
                kind: AttentionKind::System,
                reason: safe(&path),
                target: Some(AttentionTarget::Result {
                    primary: PrimaryResult::Artifact(path),
                }),
                status_unknown: false,
            });
        }
    }
    Ok(())
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
    fn recent_results_fit_the_remaining_budget_and_do_not_displace_questions() {
        use usagi_core::domain::agent::{DispatchBinding, DispatchRun, RunStatus, WorkerRef};
        use usagi_core::infrastructure::store::dispatch::DispatchStore;
        let root = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(root.path().join("dispatch"));
        let entry = session();
        let dir = root.path().join(".usagi/sessions/build/artifacts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.md"), "verified").unwrap();
        std::fs::write(dir.join(".index.json"), r#"["report.md"]"#).unwrap();
        let agent = AgentId::new();
        let workspace = WorkspaceId::new();
        let mut snapshot =
            project(workspace, &[], &[], &std::collections::BTreeMap::new(), &[]).unwrap();
        let base_time = chrono::Utc::now();
        for (offset, primary) in [
            (1, PrimaryResult::Artifact("report.md".into())),
            (
                0,
                PrimaryResult::PullRequest(
                    usagi_core::domain::pr_inventory::canonicalize("https://github.com/a/b/pull/2")
                        .unwrap(),
                ),
            ),
        ] {
            let run = OperationId::new();
            store
                .upsert_run(DispatchRun {
                    run_id: run,
                    agent_id: agent,
                    prompt: "test".into(),
                    started_at: base_time,
                    ended_at: None,
                    status: RunStatus::Running,
                })
                .unwrap();
            store
                .upsert_binding(DispatchBinding {
                    run_id: run,
                    caller: CallerRef {
                        session_id: None,
                        agent_id: agent,
                    },
                    worker: WorkerRef {
                        session_id: Some(entry.session_id),
                        agent_id: agent,
                    },
                })
                .unwrap();
            store
                .reconcile_report_outcome(
                    run,
                    agent,
                    RunStatus::Completed,
                    AgentStatus::Idle,
                    base_time + chrono::Duration::seconds(offset),
                    Some(primary),
                )
                .unwrap();
        }
        add_results(
            &mut snapshot,
            root.path(),
            std::slice::from_ref(&entry),
            &store,
        )
        .unwrap();
        assert_eq!(
            snapshot
                .items
                .iter()
                .map(|r| r.reason.as_str())
                .collect::<Vec<_>>(),
            ["report.md", "https://github.com/a/b/pull/2"]
        );
        let question = session_item(&entry, AttentionKind::Blocked, "needs action");
        snapshot.items = vec![question; ATTENTION_ITEMS_MAX - 1];
        add_results(&mut snapshot, root.path(), &[entry], &store).unwrap();
        assert_eq!(snapshot.items.len(), ATTENTION_ITEMS_MAX);
        assert_eq!(snapshot.items.last().unwrap().reason, "report.md");
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
        assert_eq!(
            project(
                workspace,
                &sessions[..1],
                &[],
                &std::collections::BTreeMap::default(),
                &[AgentPhase::Absent]
            )
            .unwrap()
            .items,
            [] as [usagi_core::domain::attention::AttentionItem; 0]
        );
    }
}

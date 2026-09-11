//! Human session workflow composition. No Agent credentials are accepted here.
use super::{
    AgentProfileId, ConnectionWorkspace, DaemonRequest, SharedAgentRuntime, SharedPrInventory,
    envelope, run_agent_readiness,
};
use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
use usagi_core::domain::workflow::{Delivery, WorkflowCommand};
use usagi_core::infrastructure::client::AgentLaunchIntent;
use usagi_core::infrastructure::ipc::{
    Envelope, ErrorCode, ProtocolError, RequestId, ResponseOutcome, ServerHello,
};
use usagi_daemon::usecase::agent_ipc::SessionScopeResolver;
use usagi_daemon::usecase::workflow;

fn unavailable(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(ErrorCode::Unavailable, format!("Workflow: {error}"))
}

pub(super) fn dispatch(
    agent: &SharedAgentRuntime,
    inventory: &SharedPrInventory,
    bound: &ConnectionWorkspace,
    request_id: RequestId,
    request: DaemonRequest,
    hello: &ServerHello,
) -> Envelope {
    let result = handle(agent, inventory, bound, request);
    match result {
        Ok(value) => envelope(hello, request_id, ResponseOutcome::Ok, value),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

fn handle(
    agent: &SharedAgentRuntime,
    inventory: &SharedPrInventory,
    bound: &ConnectionWorkspace,
    request: DaemonRequest,
) -> Result<serde_json::Value, ProtocolError> {
    let (workspace, session, control) = match request {
        DaemonRequest::WorkflowSnapshot { workspace, session } => (workspace, session, None),
        DaemonRequest::WorkflowControl {
            workspace,
            session,
            operation_id,
            command,
        } => (workspace, session, Some((operation_id, command))),
        _ => {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid Workflow request",
            ));
        }
    };
    let actual = bound
        .sessions()
        .lock()
        .map_err(unavailable)?
        .workspace_id()
        .map_err(|_| unavailable("workspace identity is unavailable"))?;
    if actual != workspace {
        return Err(ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "Workflow belongs to another workspace",
        ));
    }
    let scope = bound.scope_resolver();
    scope
        .resolve_available_scope(workspace, Some(session))
        .map_err(|_| unavailable("session is unavailable"))?;
    let store = agent.lock().map_err(unavailable)?.dispatch_store().clone();
    if let Some((operation, command)) = control {
        if matches!(command, WorkflowCommand::Instruct { .. }) {
            workflow::snapshot(&store, workspace, session).map_err(unavailable)?;
        }
        workflow::admit(&store, workspace, session, operation, &command)
            .map_err(|error| admission_error(&error))?;
        match command {
            WorkflowCommand::Start { goal } => {
                start(agent, bound, workspace, session, operation, &goal)?;
            }
            WorkflowCommand::Instruct { .. } => deliver(agent, workspace, session, operation)?,
        }
    }
    let snapshot = workflow::snapshot(&store, workspace, session).map_err(unavailable)?;
    if let Some(run) = &snapshot.run {
        for instruction in &run.instructions {
            if instruction.delivery == Delivery::Queued {
                let _ = deliver(agent, workspace, session, instruction.id);
            }
        }
        verify_progress(&store, inventory, bound, workspace, session, run)?;
    }
    serde_json::to_value(workflow::snapshot(&store, workspace, session).map_err(unavailable)?)
        .map_err(unavailable)
}

fn verify_progress(
    store: &usagi_core::infrastructure::store::dispatch::DispatchStore,
    inventory: &SharedPrInventory,
    bound: &ConnectionWorkspace,
    workspace: WorkspaceId,
    session: SessionId,
    run: &usagi_core::domain::workflow::WorkflowRun,
) -> Result<(), ProtocolError> {
    let scope = bound.scope_resolver();
    if matches!(
        run.phase,
        usagi_core::domain::workflow::Phase::Verifying | usagi_core::domain::workflow::Phase::Ready
    ) && let Some(review) = &run.review
    {
        let entries = inventory
            .lock()
            .map_err(unavailable)?
            .snapshot(session)
            .map_err(unavailable)?
            .entries;
        let directory = scope
            .resolve_available_scope(workspace, Some(session))
            .map_err(|_| unavailable("session is unavailable"))?
            .working_directory;
        let verified = workflow::verify_pr(
            &super::SystemGit,
            &mut super::GhProcess,
            &directory,
            &review.target,
            &entries,
        );
        publish_verification(store, workspace, session, run, verified)?;
    }
    Ok(())
}

fn publish_verification(
    store: &usagi_core::infrastructure::store::dispatch::DispatchStore,
    workspace: WorkspaceId,
    session: SessionId,
    run: &usagi_core::domain::workflow::WorkflowRun,
    verified: Result<(), &'static str>,
) -> Result<(), ProtocolError> {
    let review = run
        .review
        .as_ref()
        .ok_or_else(|| unavailable("review is missing"))?;
    store
        .update_workflow(workspace, session, |record| {
            if let Some(current) = record.as_mut().and_then(|record| record.run.as_mut())
                && current.id == run.id
                && current.review == run.review
            {
                current.phase = usagi_core::domain::workflow::Phase::Verifying;
                match verified {
                    Ok(()) => {
                        current
                            .mark_ready(&review.target.head_sha, true, true)
                            .map_err(anyhow::Error::msg)?;
                        current.waiting_reason = None;
                    }
                    Err(reason) => {
                        current.waiting_reason = Some(reason.into());
                        if reason.contains("HEAD changed") {
                            current.phase = usagi_core::domain::workflow::Phase::Revising;
                        }
                    }
                }
            }
            Ok(())
        })
        .map_err(unavailable)
}

fn admission_error(error: &anyhow::Error) -> ProtocolError {
    let message = error.to_string();
    let code = match message.as_str() {
        "invalid workflow goal"
        | "workflow has not started"
        | "workflow launch is not yet admitted"
        | "reviewer is not assigned yet"
        | "workflow capacity exhausted"
        | "instruction is empty, invalid, or the journal is full" => ErrorCode::InvalidArgument,
        "session already has another workflow"
        | "instruction ID conflicts with workflow start"
        | "instruction ID conflicts with an existing instruction" => ErrorCode::IdempotencyConflict,
        _ => ErrorCode::Unavailable,
    };
    ProtocolError::new(code, message)
}

fn start(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    workspace: WorkspaceId,
    session: SessionId,
    operation: OperationId,
    goal: &str,
) -> Result<(), ProtocolError> {
    let intent = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new("codex").map_err(unavailable)?),
    };
    let prompt = workflow::initial_prompt(goal);
    let preflight = agent
        .lock()
        .map_err(unavailable)?
        .prepare_workflow_readiness(&operation.to_string(), &intent, &prompt)?;
    run_agent_readiness(agent, preflight.as_ref())?;
    let mut owner = agent.lock().map_err(unavailable)?;
    owner.launch_workflow_after_readiness(
        &operation.to_string(),
        &intent,
        &prompt,
        &bound.scope_resolver(),
        preflight.as_ref(),
    )?;
    let store = owner.dispatch_store();
    let binding = store
        .binding(operation)
        .map_err(unavailable)?
        .ok_or_else(|| unavailable("admitted Agent identity is unavailable"))?;
    workflow::bind(
        store,
        workspace,
        session,
        operation,
        binding.worker.agent_id,
    )
    .map_err(unavailable)
}

fn deliver(
    agent: &SharedAgentRuntime,
    workspace: WorkspaceId,
    session: SessionId,
    operation: OperationId,
) -> Result<(), ProtocolError> {
    let mut owner = agent.lock().map_err(unavailable)?;
    let store = owner.dispatch_store().clone();
    let record = store
        .workflow(workspace, session)
        .map_err(unavailable)?
        .ok_or_else(|| unavailable("workflow disappeared"))?;
    let instruction = record
        .run
        .as_ref()
        .and_then(|run| run.instructions.iter().find(|item| item.id == operation))
        .ok_or_else(|| unavailable("instruction disappeared"))?;
    if instruction.delivery != Delivery::Queued {
        return Ok(());
    }
    let recipient = store
        .agent_in_workspace(workspace, instruction.recipient)
        .map_err(unavailable)?
        .filter(|worker| worker.session_id == Some(session))
        .ok_or_else(|| unavailable("instruction recipient is unavailable"))?;
    let Some(run) = recipient.current_run else {
        return Ok(());
    };
    let prompt = format!(
        "Workflow instruction {operation} (process this ID once):\n{}",
        instruction.body
    );
    store
        .update_workflow(workspace, session, |record| {
            let item = record
                .as_mut()
                .and_then(|value| value.run.as_mut())
                .and_then(|run| {
                    run.instructions
                        .iter_mut()
                        .find(|item| item.id == operation)
                })
                .ok_or_else(|| anyhow::anyhow!("instruction disappeared"))?;
            item.delivery = Delivery::Unconfirmed;
            Ok(())
        })
        .map_err(unavailable)?;
    owner.prompt_run(run, &prompt)?;
    store
        .update_workflow(workspace, session, |record| {
            let item = record
                .as_mut()
                .and_then(|value| value.run.as_mut())
                .and_then(|run| {
                    run.instructions
                        .iter_mut()
                        .find(|item| item.id == operation)
                })
                .ok_or_else(|| anyhow::anyhow!("instruction disappeared"))?;
            item.delivery = Delivery::Notified;
            Ok(())
        })
        .map_err(unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::agent_message::ReviewTarget;
    use usagi_core::domain::id::AgentId;
    use usagi_core::domain::workflow::{Phase, Review};
    use usagi_core::infrastructure::store::dispatch::DispatchStore;

    #[test]
    fn workflow_error_mapping_distinguishes_refusal_from_unknown_effect() {
        for message in [
            "invalid workflow goal",
            "workflow has not started",
            "workflow launch is not yet admitted",
            "reviewer is not assigned yet",
            "instruction is empty, invalid, or the journal is full",
        ] {
            assert_eq!(
                admission_error(&anyhow::anyhow!(message)).code,
                ErrorCode::InvalidArgument
            );
        }
        for message in [
            "session already has another workflow",
            "instruction ID conflicts with workflow start",
            "instruction ID conflicts with an existing instruction",
        ] {
            assert_eq!(
                admission_error(&anyhow::anyhow!(message)).code,
                ErrorCode::IdempotencyConflict
            );
        }
        assert_eq!(
            admission_error(&anyhow::anyhow!("storage failed")).code,
            ErrorCode::Unavailable
        );
        assert!(unavailable("storage failed").message.contains("Workflow:"));
    }

    #[test]
    fn workflow_verification_publication_fences_concurrent_reviews_and_invalidates_ready() {
        let directory = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(directory.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        workflow::admit(
            &store,
            workspace,
            session,
            operation,
            &WorkflowCommand::Start {
                goal: "task".into(),
            },
        )
        .unwrap();
        workflow::bind(&store, workspace, session, operation, AgentId::new()).unwrap();
        let mut run = store
            .workflow(workspace, session)
            .unwrap()
            .unwrap()
            .run
            .unwrap();
        assert!(publish_verification(&store, workspace, session, &run, Ok(())).is_err());
        run.phase = Phase::Verifying;
        run.review = Some(Review {
            request: OperationId::new(),
            target: ReviewTarget {
                base_sha: "a".repeat(40),
                head_sha: "b".repeat(40),
            },
            approved: true,
        });
        store
            .update_workflow(workspace, session, |record| {
                record.as_mut().unwrap().run = Some(run.clone());
                Ok(())
            })
            .unwrap();
        publish_verification(&store, workspace, session, &run, Ok(())).unwrap();
        assert_eq!(
            store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Ready
        );
        publish_verification(&store, workspace, session, &run, Err("checks pending")).unwrap();
        let pending = store
            .workflow(workspace, session)
            .unwrap()
            .unwrap()
            .run
            .unwrap();
        assert_eq!(pending.phase, Phase::Verifying);
        assert_eq!(pending.waiting_reason.as_deref(), Some("checks pending"));
        publish_verification(&store, workspace, session, &run, Err("HEAD changed")).unwrap();
        assert_eq!(
            store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Revising
        );
        let mut stale = run.clone();
        stale.id = OperationId::new();
        publish_verification(&store, workspace, session, &stale, Ok(())).unwrap();
        stale = run.clone();
        stale.review.as_mut().unwrap().request = OperationId::new();
        publish_verification(&store, workspace, session, &stale, Ok(())).unwrap();
        assert_eq!(
            store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Revising
        );
        assert!(publish_verification(&store, workspace, SessionId::new(), &run, Ok(())).is_err());
    }
}

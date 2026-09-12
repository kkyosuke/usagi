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

fn unavailable_scope<T>(_: T) -> ProtocolError {
    unavailable("session/workspace identity is unavailable")
}

fn unavailable(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(ErrorCode::Unavailable, format!("Workflow: {error}"))
}

pub(super) fn dispatch(
    agent: &SharedAgentRuntime,
    inventory: &SharedPrInventory,
    bound: &ConnectionWorkspace,
    request_id: RequestId,
    request: DaemonRequest,
    raw: &serde_json::Value,
    hello: &ServerHello,
) -> Envelope {
    let result = reject_agent_context(raw).and_then(|()| handle(agent, inventory, bound, request));
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

fn reject_agent_context(raw: &serde_json::Value) -> Result<(), ProtocolError> {
    if raw.get("caller_context").is_some()
        || raw.get("_caller_credential").is_some()
        || raw
            .get("payload")
            .and_then(|value| value.get("_caller_credential"))
            .is_some()
    {
        return Err(ProtocolError::new(
            ErrorCode::PermissionDenied,
            "Workflow controls are human-only",
        ));
    }
    Ok(())
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
        .map_err(unavailable_scope)?;
    if actual != workspace {
        return Err(ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "Workflow belongs to another workspace",
        ));
    }
    let scope = bound.scope_resolver();
    scope
        .resolve_available_scope(workspace, Some(session))
        .map_err(unavailable_scope)?;
    let store = agent.lock().map_err(unavailable)?.dispatch_store().clone();
    if let Some((operation, command)) = control {
        // Reconcile immediately before admission so the command is judged
        // against current evidence, and keep PR verification out of its way: a
        // GitHub read that is momentarily unavailable must not refuse an
        // instruction.
        reconcile(agent, workspace, session)?;
        workflow::admit(&store, workspace, session, operation, &command)
            .map_err(|error| admission_error(&error))?;
        match command {
            WorkflowCommand::Start { goal, agents } => {
                if let Err(error) =
                    start(agent, bound, workspace, session, operation, &goal, agents)
                {
                    store
                        .update_workflow(workspace, session, |record| {
                            if let Some(record) = record {
                                record.start_error = Some(error.message.clone());
                            }
                            Ok(())
                        })
                        .map_err(unavailable)?;
                    return Err(error);
                }
            }
            WorkflowCommand::Instruct { .. } => deliver(agent, workspace, session, operation)?,
        }
        // The command changed the record, not the peer journal, so the answer
        // is a stored projection rather than a second replay.
        return serde_json::to_value(
            workflow::projection(&store, workspace, session).map_err(unavailable)?,
        )
        .map_err(unavailable);
    }
    // One reconcile pass per request. The resident lane owns progress; an open
    // tab only has to observe it.
    serde_json::to_value(advance(
        agent,
        inventory,
        &scope,
        workspace,
        session,
        Attention::Requested,
    )?)
    .map_err(unavailable)
}

/// Reflect observed evidence in the stored run: replay the peer journal, then
/// decide whether the assigned participant is still running.
fn reconcile(
    agent: &SharedAgentRuntime,
    workspace: WorkspaceId,
    session: SessionId,
) -> Result<(), ProtocolError> {
    synchronized_snapshot(agent, workspace, session)?;
    reconcile_runtime(agent, workspace, session)
}

/// Who is carrying the run forward.
///
/// Both reconcile the journal and re-deliver queued instructions. They differ
/// only over a run that already reached `PR ready`: a person looking at the tab
/// still wants a stale approval invalidated, but an unattended sweep must not
/// re-verify a finished PR every tick — it would query GitHub forever and demote
/// the run the moment the branch moves on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attention {
    Unattended,
    Requested,
}

impl Attention {
    fn verifies(self, phase: usagi_core::domain::workflow::Phase) -> bool {
        self == Self::Requested || phase != usagi_core::domain::workflow::Phase::Ready
    }
}

/// Carry one run as far as observed evidence allows: reconcile the peer journal,
/// re-deliver queued instructions and verify an approved PR.
///
/// The resident lane and every request share this one pass, so a workflow makes
/// the same progress whether or not its tab is open.
pub(super) fn advance(
    agent: &SharedAgentRuntime,
    inventory: &SharedPrInventory,
    scope: &dyn SessionScopeResolver,
    workspace: WorkspaceId,
    session: SessionId,
    attention: Attention,
) -> Result<usagi_core::domain::workflow::WorkflowSnapshot, ProtocolError> {
    let store = agent.lock().map_err(unavailable)?.dispatch_store().clone();
    reconcile(agent, workspace, session)?;
    let snapshot = workflow::projection(&store, workspace, session).map_err(unavailable)?;
    if let Some(run) = &snapshot.run {
        for instruction in &run.instructions {
            if instruction.delivery == Delivery::Queued {
                let _ = deliver(agent, workspace, session, instruction.id);
            }
        }
        if attention.verifies(run.phase) {
            verify_progress(
                &store,
                inventory,
                scope,
                workspace,
                session,
                run,
                &super::SystemGit,
                &mut super::GhProcess,
            )?;
        }
        return workflow::projection(&store, workspace, session).map_err(unavailable);
    }
    Ok(snapshot)
}

fn synchronized_snapshot(
    agent: &SharedAgentRuntime,
    workspace: WorkspaceId,
    session: SessionId,
) -> Result<usagi_core::domain::workflow::WorkflowSnapshot, ProtocolError> {
    let owner = agent.lock().map_err(unavailable)?;
    let store = owner.dispatch_store();
    let Some(record) = store.workflow(workspace, session).map_err(unavailable)? else {
        return workflow::snapshot(store, workspace, session).map_err(unavailable);
    };
    let Some(run) = record.run else {
        return workflow::snapshot(store, workspace, session).map_err(unavailable);
    };
    let mut authorized = owner
        .workflow_operation_lineage(run.id)
        .into_iter()
        .map(|operation| (run.implementer, operation))
        .collect::<Vec<_>>();
    let mut reviewer_live = false;
    for binding in store.bindings().map_err(unavailable)? {
        if binding.caller.agent_id == run.implementer
            && binding.caller.session_id == Some(session)
            && binding.worker.session_id == Some(session)
            && binding.worker.agent_id != run.implementer
        {
            if Some(binding.worker.agent_id) == run.reviewer {
                reviewer_live |= owner.workflow_live_operation(binding.run_id).is_some();
            }
            authorized.extend(
                owner
                    .workflow_operation_lineage(binding.run_id)
                    .into_iter()
                    .map(|operation| (binding.worker.agent_id, operation)),
            );
        }
    }
    let selected_live =
        if record.suspended_phase == Some(usagi_core::domain::workflow::Phase::Reviewing) {
            reviewer_live
        } else {
            owner.workflow_live_operation(run.id).is_some()
        };
    store
        .update_workflow(workspace, session, |value| {
            let record = value
                .as_mut()
                .ok_or(anyhow::anyhow!("workflow disappeared"))?;
            let mut changed = false;
            for proof in authorized {
                if !record.authorized_operations.contains(&proof) {
                    record.authorized_operations.push(proof);
                    changed = true;
                }
            }
            // Restore before consuming evidence, including a resumed participant
            // that sent a valid verdict and already exited between observations.
            if (changed || selected_live)
                && let Some(previous) = record.suspended_phase.take()
                && let Some(current) = record.run.as_mut()
            {
                current.phase = previous;
                current.waiting_reason = None;
            }
            Ok(())
        })
        .map_err(unavailable)?;
    // Resume admission and message dispatch use this same runtime owner lock.
    workflow::snapshot(store, workspace, session).map_err(unavailable)
}

pub(super) fn reconcile_runtime(
    agent: &SharedAgentRuntime,
    workspace: WorkspaceId,
    session: SessionId,
) -> Result<(), ProtocolError> {
    use usagi_core::domain::workflow::Phase;
    let owner = agent.lock().map_err(unavailable)?;
    let store = owner.dispatch_store();
    let Some(record) = store.workflow(workspace, session).map_err(unavailable)? else {
        return Ok(());
    };
    let Some(run) = record.run else {
        return Ok(());
    };
    let phase = record.suspended_phase.unwrap_or(run.phase);
    if !matches!(
        phase,
        Phase::Implementing | Phase::Revising | Phase::Reviewing
    ) {
        return Ok(());
    }
    let implementation = owner.workflow_live_operation(run.id);
    let selected = if phase == Phase::Reviewing {
        store
            .bindings()
            .map_err(unavailable)?
            .iter()
            .find(|binding| {
                Some(binding.worker.agent_id) == run.reviewer
                    && binding.worker.session_id == Some(session)
                    && binding.caller.agent_id == run.implementer
                    && binding.caller.session_id == Some(session)
            })
            .and_then(|binding| owner.workflow_live_operation(binding.run_id))
    } else {
        implementation
    };
    store.update_workflow(workspace,session,|record| {
        let record=record.as_mut().ok_or(anyhow::anyhow!("workflow disappeared"))?;
        let current=record.run.as_mut().ok_or(anyhow::anyhow!("workflow run disappeared"))?;
        if current.id!=run.id || current.phase!=run.phase || current.review!=run.review {return Ok(());}
        record.implementation_operation=implementation;
        if selected.is_none() {
            record.suspended_phase=Some(phase);
            current.phase=Phase::Waiting;
            current.waiting_reason=Some("Assigned Agent is stopped or interrupted. Recover that exact Agent from the agent menu; workflow state is retained.".into());
        } else if let Some(previous)=record.suspended_phase.take() {
            current.phase=previous;
            current.waiting_reason=None;
        }
        Ok(())
    }).map_err(unavailable)
}

#[allow(clippy::too_many_arguments)] // Keep both external verification ports explicit and injectable.
pub(super) fn verify_progress(
    store: &usagi_core::infrastructure::store::dispatch::DispatchStore,
    inventory: &SharedPrInventory,
    scope: &dyn SessionScopeResolver,
    workspace: WorkspaceId,
    session: SessionId,
    run: &usagi_core::domain::workflow::WorkflowRun,
    git: &dyn usagi_core::infrastructure::git::GitRunner,
    gh: &mut dyn usagi_daemon::usecase::pr_inventory::GhProcessPort<Error = std::io::Error>,
) -> Result<(), ProtocolError> {
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
            .map_err(unavailable_scope)?
            .working_directory;
        let verified = workflow::verify_pr(git, gh, &directory, &review.target, &entries);
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
        .ok_or(unavailable("review is missing"))?;
    store
        .update_workflow(workspace, session, |record| {
            if let Some(current) = record.as_mut().and_then(|record| record.run.as_mut())
                && current.id == run.id
                && current.review == run.review
                // The lane and an open tab verify the same run concurrently, so
                // publish only onto the phase this verification was started from.
                && current.phase == run.phase
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

/// Advance every stored run once, independent of any client connection.
///
/// Progress used to happen only inside a Workflow request, so a run stopped
/// moving whenever its tab was not the active one — and never reached
/// `PR ready` with the TUI closed. This sweep is what makes a workflow a
/// background activity rather than a foreground animation.
///
/// One unreadable or unresolvable session is skipped, never fatal: the other
/// runs in the same sweep must still advance.
pub(super) fn sweep(
    agent: &SharedAgentRuntime,
    inventory: &SharedPrInventory,
    scope: &dyn SessionScopeResolver,
    stopping: &dyn Fn() -> bool,
) -> Result<usize, ProtocolError> {
    let store = agent.lock().map_err(unavailable)?.dispatch_store().clone();
    let mut advanced = 0;
    for (workspace, session) in store.workflow_sessions() {
        // Verification shells out to git and GitHub, so a long sweep must not
        // become a long shutdown.
        if stopping() {
            break;
        }
        // A session whose worktree is gone (removed, or owned by a workspace this
        // daemon no longer holds) keeps its record but has nothing to advance.
        if scope
            .resolve_available_scope(workspace, Some(session))
            .is_err()
        {
            continue;
        }
        if !advanceable(&store, workspace, session) {
            continue;
        }
        if advance(
            agent,
            inventory,
            scope,
            workspace,
            session,
            Attention::Unattended,
        )
        .is_ok()
        {
            advanced += 1;
        }
    }
    Ok(advanced)
}

/// Whether an unattended sweep has anything to do for this record.
///
/// A launch that never bound its Agent waits for the human to retry it; there is
/// no run to reconcile, and sweeping it would rewrite the record every tick for
/// no progress. Every bound run is swept, including one at `PR ready`: a
/// reviewer can still be asked for another review from there, and instructions
/// enqueued there still have to reach their recipient.
fn advanceable(
    store: &usagi_core::infrastructure::store::dispatch::DispatchStore,
    workspace: WorkspaceId,
    session: SessionId,
) -> bool {
    store
        .workflow(workspace, session)
        .is_ok_and(|record| record.is_some_and(|record| record.run.is_some()))
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
    agents: usagi_core::domain::workflow::WorkflowAgents,
) -> Result<(), ProtocolError> {
    let intent = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new(agents.implementer.profile_id()).map_err(unavailable)?),
    };
    let prompt = workflow::initial_prompt(goal, agents);
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
        .ok_or(unavailable("admitted Agent identity is unavailable"))?;
    workflow::bind(
        store,
        workspace,
        session,
        operation,
        binding.worker.agent_id,
    )
    .map_err(unavailable)?;
    store
        .remember_workflow_start(workspace, session)
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
        .ok_or(unavailable("workflow disappeared"))?;
    let instruction = record
        .run
        .as_ref()
        .and_then(|run| run.instructions.iter().find(|item| item.id == operation))
        .ok_or(unavailable("instruction disappeared"))?;
    if instruction.delivery != Delivery::Queued {
        return Ok(());
    }
    let recipient = store
        .agent_in_workspace(workspace, instruction.recipient)
        .map_err(unavailable)?
        .filter(|worker| worker.session_id == Some(session))
        .ok_or(unavailable("instruction recipient is unavailable"))?;
    let Some(run) = recipient.current_run else {
        return Ok(());
    };
    if !record
        .authorized_operations
        .contains(&(instruction.recipient, run))
    {
        // Reusing a stopped Agent identity for a fresh launch is not an exact
        // workflow resume. Keep the durable instruction queued for its lineage.
        return Ok(());
    }
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
                .ok_or(anyhow::anyhow!("instruction disappeared"))?;
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
                .ok_or(anyhow::anyhow!("instruction disappeared"))?;
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
    use usagi_core::domain::workflow::{Phase, Review, WorkflowRun};
    use usagi_core::infrastructure::store::dispatch::DispatchStore;

    #[test]
    fn workflow_error_mapping_distinguishes_refusal_from_unknown_effect() {
        assert!(reject_agent_context(&serde_json::json!({})).is_ok());
        for raw in [
            serde_json::json!({"caller_context":{"credential":"valid"}}),
            serde_json::json!({"caller_context":{"credential":"invalid"}}),
            serde_json::json!({"caller_context":null}),
            serde_json::json!({"_caller_credential":"valid"}),
            serde_json::json!({"payload":{"_caller_credential":"valid"}}),
        ] {
            assert_eq!(
                reject_agent_context(&raw).unwrap_err().code,
                ErrorCode::PermissionDenied
            );
        }
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
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
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
        // Each verification starts from the run as it is stored now, exactly as
        // the lane and a request both do; a publication onto a phase that moved
        // in between is refused below.
        let stored = |phase: Phase| {
            let run: WorkflowRun = store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .run
                .unwrap();
            assert_eq!(run.phase, phase);
            run
        };
        let ready = stored(Phase::Ready);
        publish_verification(&store, workspace, session, &ready, Err("checks pending")).unwrap();
        let pending = stored(Phase::Verifying);
        assert_eq!(pending.waiting_reason.as_deref(), Some("checks pending"));
        publish_verification(&store, workspace, session, &pending, Err("HEAD changed")).unwrap();
        let revising = stored(Phase::Revising);
        // A phase that changed under a running verification is not overwritten:
        // the concurrent decision (here, a stopped participant) stands.
        store
            .update_workflow(workspace, session, |record| {
                record.as_mut().unwrap().run.as_mut().unwrap().phase = Phase::Waiting;
                Ok(())
            })
            .unwrap();
        publish_verification(&store, workspace, session, &revising, Ok(())).unwrap();
        let _ = stored(Phase::Waiting);
        store
            .update_workflow(workspace, session, |record| {
                record.as_mut().unwrap().run.as_mut().unwrap().phase = Phase::Revising;
                Ok(())
            })
            .unwrap();
        let mut stale = revising.clone();
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

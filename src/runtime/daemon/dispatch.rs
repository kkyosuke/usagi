//! Composition adapter for daemon request families and response shaping.
//!
//! Socket acceptance, process lifecycle, and worker ownership stay in the parent
//! module. This boundary binds already-admitted IPC requests to the injected
//! daemon runtimes and stores.

use super::{
    AgentAdmission, AgentDecisionWaker, AgentProfileId, AgentReadiness, AgentReadinessPreflight,
    AgentRuntime, AgentRuntimeRef, AmbiguousIssueNumber, Arc, ArtifactVerification,
    ArtifactVerificationRequest, ArtifactVerificationStatus, ArtifactVerifier, BTreeMap, BTreeSet,
    BackgroundWorker, ConnectionId, ConnectionWorkspace, CurrentLocatorFile,
    DEFAULT_GENERATION_LIMIT, DaemonRequest, DeferredDecisionWaker, Deserialize, DispatchStore,
    DispatchToolAction, Envelope, EnvelopeKind, ErrorCode, ErrorLog, FailureTransitionLog,
    GenerationFence, GenerationRegistry, GenerationRegistryFile, GhProcess, INBOX_PAGE_MAX,
    InboxCursor, InitialTask, MetricsObserver, MetricsSample, MutexGuard, OperationId, Ordering,
    Path, PathBuf, PeerProcess, PendingDaemonAgentRestart, ResponseOutcome,
    SUPERVISOR_RECOVERY_TICK, SessionId, SessionRuntimeError, SessionScopeResolver,
    SharedAgentRuntime, SharedMetricsBroker, SharedPrInventory, SharedProcessResourceSampler,
    SharedSessionRuntime, SharedSupervisorRuntime, SharedTerminalRuntime, ShutdownRequest,
    SupervisorRuntime, SupervisorToolAction, SystemGit, TeardownSignal, Tenant, TerminalId,
    TerminalPipelineMetrics, UnixStandbyProbe, UserDecisionStore, WorkspaceId, Workspaces,
    aggregate_agent_status, bounded_supervisor_query, clear_pending_daemon_agent_restart,
    current_build, observe_generation_process, output_pipeline_counters, paths,
    perform_compensating_remove, perform_create, perform_delegated_create,
    perform_remove_with_merged_head, pr_projection_counters, process_start_identity,
    recover_rollover, restore_pending_daemon_agents, rollover_trigger, validate_owned_directory,
    write_pending_daemon_agent_restart,
};

pub(super) struct DispatchToolContext<'a> {
    pub(super) agent: &'a SharedAgentRuntime,
    pub(super) terminal: &'a SharedTerminalRuntime,
    pub(super) bound: &'a ConnectionWorkspace,
    pub(super) pr_inventory: &'a SharedPrInventory,
    pub(super) decisions: &'a UserDecisionStore,
    pub(super) supervisor: &'a SharedSupervisorRuntime,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_worker_complete_reaches_the_caller_inbox
pub(super) fn dispatch_dispatch_tool(
    context: &DispatchToolContext<'_>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    let action = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::DispatchTool { action, .. } => Some(action),
            _ => None,
        });
    if action.is_some_and(|action| {
        matches!(
            action,
            DispatchToolAction::Dispatch
                | DispatchToolAction::SessionGet
                | DispatchToolAction::AgentList
                | DispatchToolAction::AgentGet
                | DispatchToolAction::TerminalList
                | DispatchToolAction::TerminalRead
                | DispatchToolAction::AgentComplete
                | DispatchToolAction::AgentFail
                | DispatchToolAction::AgentInbox
                | DispatchToolAction::AgentInboxAck
        )
    }) {
        dispatch_agent_tool(context, request_id, body, hello)
    } else {
        dispatch_user_decision(
            context.agent,
            context.bound,
            context.decisions,
            request_id,
            body,
            hello,
        )
    }
}

#[allow(clippy::too_many_lines)] // One handler keeps authentication and durable routing atomic.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_worker_complete_reaches_the_caller_inbox
pub(super) fn dispatch_agent_tool(
    context: &DispatchToolContext<'_>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use chrono::{DateTime, Utc};
    use usagi_core::domain::agent::{
        AgentProfileId, AgentStatus, InboxKind, ModelSelector, StructuredResult,
    };
    use usagi_core::domain::id::{AgentId, OperationId};
    use usagi_core::infrastructure::client::{DispatchAgentIntent, DispatchIntent};
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    #[derive(Deserialize)]
    struct SessionPayload {
        name: String,
        #[serde(default)]
        role: Option<usagi_core::domain::role::RoleId>,
    }
    #[derive(Deserialize)]
    struct DispatchPayload {
        session: SessionPayload,
        agent: serde_json::Value,
        prompt: String,
    }
    #[derive(Deserialize)]
    struct AgentIdPayload {
        agent_id: AgentId,
    }
    #[derive(Deserialize)]
    struct ReportPayload {
        summary: String,
        #[serde(default)]
        result: Option<StructuredResult>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        run_id: Option<OperationId>,
    }
    #[derive(Deserialize)]
    struct InboxPayload {
        #[serde(default)]
        cursor: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        since: Option<DateTime<Utc>>,
        #[serde(default)]
        unread_only: bool,
    }
    #[derive(Deserialize)]
    struct InboxAckPayload {
        cursor: u64,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TerminalReadPayload {
        terminal_id: TerminalId,
        #[serde(default = "default_terminal_read_lines")]
        lines: usize,
    }

    fn default_terminal_read_lines() -> usize {
        usagi_core::usecase::terminal_observation::TERMINAL_READ_DEFAULT_LINES
    }

    let agent = context.agent;
    let terminal = context.terminal;
    let bound = context.bound;
    let pr_inventory = context.pr_inventory;
    let supervisor = context.supervisor;

    let parsed = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::DispatchTool {
                action,
                operation_id,
                payload,
                caller_context,
            } => Some((action, operation_id, payload, caller_context)),
            _ => None,
        });
    let Some((action, operation_id, payload, caller_context)) = parsed else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let response = (|| -> Result<(ResponseOutcome, serde_json::Value), ProtocolError> {
        let credential = caller_context
            .as_ref()
            .filter(|context| !context.credential.is_empty())
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "agent caller provenance is unknown",
                )
            })?;
        let snapshot = bound
            .sessions()
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
            })?
            .snapshot()
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::Unavailable,
                    "daemon could not read managed sessions",
                )
            })?;
        let workspace = snapshot
            .get("workspace_id")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::Unavailable, "workspace identity is unavailable")
            })?;
        let runtime = agent.lock().map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
        })?;
        let authenticated = runtime
            .mcp_dispatch_context(&credential.credential)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "agent caller provenance is unknown",
                )
            })?;
        if authenticated.workspace_id != workspace {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent caller does not belong to this workspace",
            ));
        }
        let parent_dispatch_run = authenticated.run_id;
        if matches!(
            action,
            DispatchToolAction::TerminalList | DispatchToolAction::TerminalRead
        ) {
            let scope = authenticated.terminal_scope;
            drop(runtime);
            let terminal = terminal.lock().map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "terminal owner is unavailable")
            })?;
            return match action {
                DispatchToolAction::TerminalList => {
                    if payload.as_object().is_none_or(|object| !object.is_empty()) {
                        Err(ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "terminal_list accepts no arguments",
                        ))
                    } else {
                        let terminals = terminal
                            .inventory(&scope)
                            .into_iter()
                            .map(|entry| {
                                serde_json::json!({
                                    "terminal_id": entry.terminal.terminal_id,
                                    "live": entry.live,
                                })
                            })
                            .collect::<Vec<_>>();
                        Ok((
                            ResponseOutcome::Ok,
                            serde_json::json!({"terminals": terminals}),
                        ))
                    }
                }
                DispatchToolAction::TerminalRead => {
                    let input =
                        serde_json::from_value::<TerminalReadPayload>(payload).map_err(|_| {
                            ProtocolError::new(
                                ErrorCode::InvalidArgument,
                                "invalid terminal_read payload",
                            )
                        })?;
                    if !(1..=usagi_core::usecase::terminal_observation::TERMINAL_READ_MAX_LINES)
                        .contains(&input.lines)
                    {
                        return Err(ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "terminal read line limit is out of range",
                        ));
                    }
                    let reference = terminal
                        .inventory(&scope)
                        .into_iter()
                        .find(|entry| entry.terminal.terminal_id == input.terminal_id)
                        .map(|entry| entry.terminal)
                        .ok_or_else(|| {
                            ProtocolError::new(
                                ErrorCode::NotFound,
                                "terminal was not found in the caller scope",
                            )
                        })?;
                    let snapshot = terminal.inspect(&reference)?;
                    let observation =
                        usagi_daemon::usecase::terminal_inspection::inspect_terminal(
                            snapshot,
                            input.lines,
                        )
                        .map_err(|error| match error {
                            usagi_daemon::usecase::terminal_inspection::TerminalInspectionError::InvalidLineLimit => {
                                ProtocolError::new(
                                    ErrorCode::InvalidArgument,
                                    "terminal read line limit is out of range",
                                )
                            }
                            usagi_daemon::usecase::terminal_inspection::TerminalInspectionError::InvalidCheckpoint(_) => {
                                ProtocolError::new(
                                    ErrorCode::OwnershipUnknown,
                                    "terminal snapshot is unavailable",
                                )
                            }
                        })?;
                    Ok((ResponseOutcome::Ok, serde_json::json!(observation)))
                }
                _ => unreachable!("terminal action was matched above"),
            };
        }
        let caller = authenticated.caller;
        let store = runtime.dispatch_store().clone();
        drop(runtime);
        let owned_sessions = bound
            .sessions()
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
            })?
            .created_session_ids(&caller)
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session ownership is unavailable")
            })?;
        let task_for = |agent_id: AgentId| -> Result<serde_json::Value, ProtocolError> {
            let mut runs = store
                .runs()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                })?
                .into_iter()
                .filter(|run| run.agent_id == agent_id)
                .collect::<Vec<_>>();
            runs.sort_by_key(|run| run.started_at);
            Ok(runs
                .last()
                .map_or(serde_json::Value::Null, |run| serde_json::json!(run)))
        };
        match action {
            DispatchToolAction::Dispatch => {
                let input = serde_json::from_value::<DispatchPayload>(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid session_dispatch payload",
                    )
                })?;
                let selected = if let Some(id) = input.agent.get("id") {
                    if input.agent.as_object().is_none_or(|value| value.len() != 1) {
                        return Err(ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "agent selector must use exactly one branch",
                        ));
                    }
                    DispatchAgentIntent::Existing {
                        agent_id: serde_json::from_value(id.clone()).map_err(|_| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent id")
                        })?,
                    }
                } else {
                    let object = input
                        .agent
                        .as_object()
                        .filter(|value| value.len() == 2)
                        .ok_or_else(|| {
                            ProtocolError::new(
                                ErrorCode::InvalidArgument,
                                "agent selector must use exactly one branch",
                            )
                        })?;
                    let runtime = object
                        .get("runtime")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<AgentProfileId>(value).ok())
                        .ok_or_else(|| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent runtime")
                        })?;
                    let model = object
                        .get("model")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<ModelSelector>(value).ok())
                        .ok_or_else(|| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent model")
                        })?;
                    DispatchAgentIntent::New { runtime, model }
                };
                let session_name = input.session.name;
                let requested_role = input.session.role;
                let supervision_at_preflight = supervisor
                    .lock()
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "supervisor runtime is unavailable",
                        )
                    })?
                    .supervision_fence(parent_dispatch_run)
                    .map_err(supervisor_error)?;
                if supervision_at_preflight.is_some() {
                    agent
                        .lock()
                        .map_err(|_| {
                            ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                        })?
                        .require_same_dispatch_runtime(workspace, &caller, &selected)?;
                }
                bound
                    .sessions()
                    .lock()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
                    })?
                    .authorize_create_or_reuse(&session_name, &caller)
                    .map_err(|error| {
                        let code = if matches!(error, SessionRuntimeError::PermissionDenied) {
                            ErrorCode::PermissionDenied
                        } else {
                            ErrorCode::Unavailable
                        };
                        ProtocolError::new(code, error.safe_message())
                    })?;
                let _delegation_permit = authorize_delegation(
                    bound,
                    agent,
                    Some(&caller),
                    Some(&serde_json::json!(requested_role)),
                    &operation_id,
                )
                .map_err(|error| {
                    ProtocolError::new(ErrorCode::PermissionDenied, error.safe_message())
                })?;
                let created = bound
                    .sessions()
                    .lock()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
                    })?
                    .handle(
                        usagi_core::infrastructure::client::SessionAction::Create,
                        &operation_id,
                        &serde_json::json!({
                        "name": session_name,
                        "role": requested_role,
                        "parent_session_id": caller.session_id,
                        "creator_agent_id": caller.agent_id,
                        }),
                    )
                    .map_err(|error| {
                        let code = match &error {
                            SessionRuntimeError::PermissionDenied => ErrorCode::PermissionDenied,
                            SessionRuntimeError::RoleConflict(..) => ErrorCode::RevisionConflict,
                            _ => ErrorCode::InvalidArgument,
                        };
                        ProtocolError::new(code, error.safe_message())
                    })?;
                let (session_id, parent_session_id) =
                    session_lineage_by_name(&created.body, &session_name).ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "created session is not available",
                        )
                    })?;
                agent
                    .lock()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                    })?
                    .dispatch_store()
                    .record_session_parent(workspace, session_id, parent_session_id)
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "session parentage is unavailable",
                        )
                    })?;
                let reserved_worker = if supervision_at_preflight.is_some() {
                    Some(
                        agent
                            .lock()
                            .map_err(|_| {
                                ProtocolError::new(
                                    ErrorCode::Unavailable,
                                    "agent owner is unavailable",
                                )
                            })?
                            .plan_dispatch_worker(workspace, session_id, &selected)?,
                    )
                } else {
                    None
                };
                let scope = bound.scope_resolver();
                let task_instruction = input.prompt.clone();
                let reservation = {
                    let runtime = supervisor.lock().map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "supervisor runtime is unavailable",
                        )
                    })?;
                    let supervision_before_reservation = runtime
                        .supervision_fence(parent_dispatch_run)
                        .map_err(supervisor_error)?;
                    require_stable_supervisor_fence(
                        supervision_at_preflight.as_ref(),
                        supervision_before_reservation.as_ref(),
                    )?;
                    let reservation = if let Some(reserved_worker) = reserved_worker.as_ref() {
                        runtime
                            .reserve_delegated_dispatch_for_session(
                                parent_dispatch_run,
                                &operation_id,
                                task_instruction,
                                session_id,
                                reserved_worker,
                                &session_name,
                                chrono::Utc::now(),
                            )
                            .map_err(supervisor_error)?
                    } else {
                        None
                    };
                    let supervision_after_reservation = runtime
                        .supervision_fence(parent_dispatch_run)
                        .map_err(supervisor_error)?;
                    require_stable_supervisor_fence(
                        supervision_at_preflight.as_ref(),
                        supervision_after_reservation.as_ref(),
                    )?;
                    require_supervisor_reservation_presence(
                        supervision_at_preflight.as_ref(),
                        reservation.is_some(),
                    )?;
                    reservation
                };
                let supervised = reservation.is_some();
                let prompt = reservation.map_or(input.prompt, |reservation| reservation.prompt);
                let dispatch_intent = DispatchIntent {
                    workspace,
                    session_name: session_name.clone(),
                    caller,
                    agent: selected,
                    prompt,
                };
                let admission = dispatch_agent_after_preflight(
                    agent,
                    &operation_id,
                    &dispatch_intent,
                    session_id,
                    &scope,
                    reserved_worker.as_ref(),
                );
                let admission = match admission {
                    Ok(admission) => admission,
                    Err(error) => {
                        if supervised && error.code != ErrorCode::OwnershipUnknown {
                            let failed = supervisor
                                .lock()
                                .map_err(|_| {
                                    ProtocolError::new(
                                        ErrorCode::Unavailable,
                                        "supervisor runtime is unavailable",
                                    )
                                })?
                                .fail_reserved_delegated_dispatch(
                                    &operation_id,
                                    chrono::Utc::now(),
                                );
                            if let Err(failure) = failed {
                                ErrorLog::record(&format!(
                                    "delegated Supervisor failure reconciliation deferred: {failure}"
                                ));
                            }
                        }
                        return Err(error);
                    }
                };
                if supervised
                    && let Err(error) = bind_delegated_supervisor_dispatch(
                        supervisor,
                        &admission.operation_id,
                        &admission.runtime,
                    )
                {
                    // The Agent admission is already durable. Returning an
                    // error here would invite a retry to launch duplicate work;
                    // startup/observer reconciliation binds the exact operation.
                    ErrorLog::record(&format!("delegated Supervisor promotion deferred: {error}"));
                    if let Err(reconcile) =
                        reconcile_pending_supervisor_promotions(supervisor, agent)
                    {
                        ErrorLog::record(&format!(
                            "delegated Supervisor promotion reconciliation deferred: {reconcile}"
                        ));
                    }
                }
                let runtime = agent.lock().map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })?;
                let run_id = OperationId::parse(&admission.operation_id)
                    .map_err(|_| ProtocolError::new(ErrorCode::Internal, "invalid admitted run"))?;
                let run = runtime
                    .dispatch_store()
                    .runs()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                    })?
                    .into_iter()
                    .find(|run| run.run_id == run_id)
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "admitted dispatch is unavailable",
                        )
                    })?;
                Ok((
                    ResponseOutcome::Accepted {
                        operation_id: usagi_core::infrastructure::ipc::OperationId(
                            admission.operation_id.clone(),
                        ),
                        operation_revision: admission.revision,
                    },
                    serde_json::json!({"run_id": admission.operation_id, "session": session_name, "agent_id": run.agent_id, "terminal": admission.terminal, "completed": admission.completed}),
                ))
            }
            DispatchToolAction::SessionGet => {
                let input = serde_json::from_value::<SessionPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid session_get payload")
                })?;
                let session_id = session_id_by_name(&snapshot, &input.name).ok_or_else(|| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "session was not found")
                })?;
                if !owned_sessions.contains(&session_id) {
                    return Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "caller did not create the target session",
                    ));
                }
                let agents = store.agents_in_workspace(workspace).map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable"))?.into_iter().filter(|item| item.session_id == Some(session_id)).map(|item| Ok(serde_json::json!({"agent_id": item.agent_id, "runtime": item.runtime, "model": item.model, "status": item.status, "task": task_for(item.agent_id)?}))).collect::<Result<Vec<_>, ProtocolError>>()?;
                let session_metadata = snapshot
                    .get("sessions")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|items| {
                        items.iter().find(|item| {
                            item.get("session_id") == Some(&serde_json::json!(session_id))
                        })
                    });
                let role_id = session_metadata
                    .and_then(|item| item.get("role_id"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let role_summary = session_metadata
                    .and_then(|item| item.get("role_summary"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"session": input.name, "role_id": role_id, "role_summary": role_summary, "agents": agents}),
                ))
            }
            DispatchToolAction::AgentList => {
                let session = payload
                    .get("session")
                    .and_then(serde_json::Value::as_str)
                    .map(|name| {
                        session_id_by_name(&snapshot, name).ok_or_else(|| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "session was not found")
                        })
                    })
                    .transpose()?;
                if session.is_some_and(|session| !owned_sessions.contains(&session)) {
                    return Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "caller did not create the target session",
                    ));
                }
                let status = payload
                    .get("status")
                    .cloned()
                    .map(serde_json::from_value::<AgentStatus>)
                    .transpose()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent status")
                    })?;
                let agents = store.agents_in_workspace(workspace).map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable"))?.into_iter().filter(|item| item.session_id.is_some_and(|id| owned_sessions.contains(&id)) && session.is_none_or(|id| item.session_id == Some(id)) && status.is_none_or(|value| item.status == value)).map(|item| Ok(serde_json::json!({"agent_id": item.agent_id, "session_id": item.session_id, "runtime": item.runtime, "model": item.model, "status": item.status, "task": task_for(item.agent_id)?}))).collect::<Result<Vec<_>, ProtocolError>>()?;
                Ok((ResponseOutcome::Ok, serde_json::json!({"agents": agents})))
            }
            DispatchToolAction::AgentGet => {
                let input = serde_json::from_value::<AgentIdPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent_get payload")
                })?;
                let item = store
                    .agent_in_workspace(workspace, input.agent_id)
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                    })?
                    .ok_or_else(|| {
                        ProtocolError::new(ErrorCode::InvalidArgument, "agent was not found")
                    })?;
                if item
                    .session_id
                    .is_none_or(|session| !owned_sessions.contains(&session))
                {
                    return Err(ProtocolError::new(
                        ErrorCode::PermissionDenied,
                        "caller did not create the target session",
                    ));
                }
                let runs = store
                    .runs()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                    })?
                    .into_iter()
                    .filter(|run| run.agent_id == item.agent_id)
                    .collect::<Vec<_>>();
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"agent": item, "runs": runs}),
                ))
            }
            DispatchToolAction::AgentComplete | DispatchToolAction::AgentFail => {
                let input = serde_json::from_value::<ReportPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent report payload")
                })?;
                if input.summary.trim().is_empty() {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "report summary must not be empty",
                    ));
                }
                let kind = if action == DispatchToolAction::AgentComplete {
                    InboxKind::Completed
                } else {
                    InboxKind::Failed
                };
                let summary = input
                    .error
                    .filter(|_| kind == InboxKind::Failed)
                    .map_or(input.summary.clone(), |error| {
                        format!("{}: {error}", input.summary)
                    });
                let reported_result = input.result.clone();
                let mut runtime = agent.lock().map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })?;
                let delivery = runtime.report_from_mcp(
                    &credential.credential,
                    input.run_id,
                    kind,
                    summary,
                    input.result,
                )?;
                drop(runtime);
                let completed = kind == InboxKind::Completed
                    && delivery
                        .committed
                        .as_ref()
                        .is_some_and(|message| message.kind == InboxKind::Completed);
                let reported_pr = completed
                    .then_some(reported_result.as_ref())
                    .flatten()
                    .and_then(|result| result.pr.as_deref());
                project_reported_pr(pr_inventory, delivery.worker.session_id, reported_pr)
                    .inspect_err(|_| ErrorLog::record("reported PR projection failed"))?;
                if completed {
                    verify_completed_goal_artifact(
                        supervisor,
                        &bound.workspaces,
                        delivery.committed.as_ref().map(|message| message.run_id),
                        GoalArtifactReport::Fresh(reported_result),
                    )?;
                }
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"delivered_to": delivery.delivered_to}),
                ))
            }
            DispatchToolAction::AgentInbox => {
                let input = serde_json::from_value::<InboxPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent_inbox payload")
                })?;
                let page = store
                    .inbox_page(
                        &caller,
                        input
                            .cursor
                            .map(|next_sequence| InboxCursor { next_sequence }),
                        input.limit.unwrap_or(INBOX_PAGE_MAX),
                        input.unread_only,
                        input.since,
                    )
                    .map_err(|error| map_inbox_query_error(&error))?;
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({
                        "messages": page.messages,
                        "next_cursor": page.next_cursor.next_sequence,
                        "has_more": page.has_more,
                    }),
                ))
            }
            DispatchToolAction::AgentInboxAck => {
                let input = serde_json::from_value::<InboxAckPayload>(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid agent_inbox_ack payload",
                    )
                })?;
                let cursor = store
                    .ack_inbox(
                        &caller,
                        InboxCursor {
                            next_sequence: input.cursor,
                        },
                    )
                    .map_err(|error| map_inbox_query_error(&error))?;
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"acked_cursor": cursor.next_sequence}),
                ))
            }
            _ => Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid agent tool action",
            )),
        }
    })();
    match response {
        Ok((outcome, body)) => envelope(hello, request_id, outcome, body),
        Err(error) => envelope(
            hello,
            request_id,
            usagi_core::infrastructure::ipc::ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=pr_snapshot_events_cover_success_scoped_and_lane_errors
pub(super) fn project_reported_pr(
    inventory: &SharedPrInventory,
    session: Option<SessionId>,
    candidate: Option<&str>,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    let (Some(session), Some(candidate)) = (session, candidate) else {
        return Ok(());
    };
    inventory
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "PR inventory is unavailable"))?
        .observe_reported(session, candidate)
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "PR projection is unavailable"))?;
    Ok(())
}

pub(super) enum GoalArtifactReport {
    Recovery,
    Fresh(Option<usagi_core::domain::agent::StructuredResult>),
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=only_an_open_non_draft_pr_with_passing_checks_satisfies_the_contract
pub(super) fn verify_completed_goal_artifact(
    supervisor: &SharedSupervisorRuntime,
    workspaces: &Workspaces,
    dispatch_run_id: Option<usagi_core::domain::id::OperationId>,
    report: GoalArtifactReport,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    use usagi_daemon::usecase::goal_artifact::GoalArtifactVerifier;

    let Some(dispatch_run_id) = dispatch_run_id else {
        return Ok(());
    };
    let request = {
        let runtime = supervisor
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "supervisor is unavailable"))?;
        if let GoalArtifactReport::Fresh(result) = report {
            runtime.prepare_artifact_verification_after_report(
                dispatch_run_id,
                result,
                chrono::Utc::now(),
            )
        } else {
            runtime.prepare_artifact_verification(dispatch_run_id, chrono::Utc::now())
        }
        .map_err(supervisor_error)?
    };
    let Some(request) = request else {
        return Ok(());
    };
    // The provider process runs outside the supervisor mutex. A slow or broken
    // remote can delay only this report request, never task/run observation.
    let Some(tenant) = workspaces.workspace(request.workspace_id) else {
        record_goal_artifact_verification(
            supervisor,
            &request,
            ArtifactVerification {
                status: ArtifactVerificationStatus::Retryable,
                result_digest: "workspace-unavailable".into(),
                safe_summary: "Goal workspace is not currently held by this daemon".into(),
            },
        )?;
        return Ok(());
    };
    let Some(request) = prepare_goal_artifact_expectation(supervisor, &tenant, request)? else {
        return Ok(());
    };
    let expectation = request.expectation.as_ref().ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::Unavailable,
            "Goal artifact expectation is unavailable",
        )
    })?;
    let verification = GoalArtifactVerifier::new(GhProcess).verify(
        request.contract,
        request.result.as_ref(),
        expectation,
        request.previous_verification_digest.as_deref(),
    );
    record_goal_artifact_verification(supervisor, &request, verification)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=workspace_git_facts_use_fixed_argv_and_are_normalized_once
pub(super) fn prepare_goal_artifact_expectation(
    supervisor: &SharedSupervisorRuntime,
    tenant: &Tenant<SharedSessionRuntime>,
    request: ArtifactVerificationRequest,
) -> Result<Option<ArtifactVerificationRequest>, usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    use usagi_daemon::usecase::goal_artifact::resolve_artifact_expectation_for_worktrees;

    if request.expectation.is_some() {
        return Ok(Some(request));
    }
    let Some(artifact_roots) = artifact_worktree_paths(tenant, &request) else {
        record_goal_artifact_verification(
            supervisor,
            &request,
            ArtifactVerification {
                status: ArtifactVerificationStatus::Retryable,
                result_digest: "artifact-worktree-unavailable".into(),
                safe_summary: "Goal artifact worktree is not currently available".into(),
            },
        )?;
        return Ok(None);
    };
    let expectation = match resolve_artifact_expectation_for_worktrees(
        &mut GhProcess,
        &artifact_roots,
        request.repository.clone(),
    ) {
        Ok(expectation) => expectation,
        Err(verification) => {
            record_goal_artifact_verification(supervisor, &request, verification)?;
            return Ok(None);
        }
    };
    let request = supervisor
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "supervisor is unavailable"))?
        .record_artifact_expectation(&request, &expectation, chrono::Utc::now())
        .map_err(supervisor_error)?;
    Ok(Some(request))
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=artifact_verification_preparation_captures_only_the_exact_completed_dispatch
pub(super) fn artifact_worktree_paths(
    tenant: &Tenant<SharedSessionRuntime>,
    request: &ArtifactVerificationRequest,
) -> Option<Vec<PathBuf>> {
    let sessions = tenant.runtime().lock().ok()?;
    let mut roots = Vec::with_capacity(request.worktrees.len());
    for source in &request.worktrees {
        let path = if let Some(session_id) = source.session_id {
            sessions
                .resolve_scope(request.workspace_id, session_id, source.worktree_id)
                .ok()?
                .path
        } else {
            (sessions.root_worktree_id() == source.worktree_id)
                .then(|| tenant.root().to_path_buf())?
        };
        roots.push(path);
    }
    roots.sort();
    roots.dedup();
    (!roots.is_empty()).then_some(roots)
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=artifact_transition_commit_failures_remain_retryable
pub(super) fn record_goal_artifact_verification(
    supervisor: &SharedSupervisorRuntime,
    request: &ArtifactVerificationRequest,
    verification: ArtifactVerification,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    supervisor
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "supervisor is unavailable"))?
        .record_artifact_verification(request, verification, chrono::Utc::now())
        .map(drop)
        .map_err(supervisor_error)
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=artifact_verification_preparation_captures_only_the_exact_completed_dispatch
pub(super) fn start_supervisor_recovery(
    supervisor: SharedSupervisorRuntime,
    agent: SharedAgentRuntime,
    workspaces: Workspaces,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-supervisor-recovery".to_owned())
        .spawn(move || {
            let worker_health =
                shutdown.monitor_background_worker(BackgroundWorker::SupervisorRecovery);
            let mut promotion_log = FailureTransitionLog::default();
            let mut worker_log = FailureTransitionLog::default();
            let mut artifact_log = FailureTransitionLog::default();
            let mut state_log = FailureTransitionLog::default();
            while !shutdown.is_requested() {
                let now = chrono::Utc::now();
                let failure = reconcile_pending_supervisor_promotions(&supervisor, &agent)
                    .err()
                    .map(|error| format!("supervisor promotion reconciliation deferred: {error}"));
                if let Some(entry) = promotion_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                let failure = reconcile_aborted_supervisor_workers(&supervisor, &agent)
                    .err()
                    .map(|error| {
                        format!("supervisor worker termination reconciliation deferred: {error}")
                    });
                if let Some(entry) = worker_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                let failure = reconcile_pending_goal_artifacts(&supervisor, &workspaces, now)
                    .err()
                    .map(|error| {
                        format!("Goal artifact verification reconciliation deferred: {error}")
                    });
                if let Some(entry) = artifact_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                let failure = supervisor.lock().map_or_else(
                    |_| {
                        Some("supervisor state reconciliation deferred: runtime unavailable".into())
                    },
                    |runtime| {
                        runtime
                            .tick_all(now, &mut AgentDecisionWaker { agent: &agent })
                            .err()
                            .map(|error| {
                                format!("supervisor state reconciliation deferred: {error}")
                            })
                    },
                );
                if let Some(entry) = state_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                if shutdown.wait_for_tick(SUPERVISOR_RECOVERY_TICK) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

pub(super) fn reconcile_aborted_supervisor_workers(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
) -> anyhow::Result<usize> {
    reconcile_supervisor_workers(supervisor, agent, None, false)
}

pub(super) fn reconcile_startup_supervisor_workers(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
) -> anyhow::Result<usize> {
    // Socket admission has not started yet, so an operation missing from the
    // hydrated Agent owner cannot appear later from a pre-crash request.
    reconcile_supervisor_workers(supervisor, agent, None, true)
}

pub(super) fn reconcile_supervisor_run_workers(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
    supervisor_run_id: usagi_core::domain::supervisor::SupervisorRunId,
) -> anyhow::Result<usize> {
    reconcile_supervisor_workers(supervisor, agent, Some(supervisor_run_id), false)
}

#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-08-31 tests=supervisor_worker_reconciliation_joins_bound_and_unbound_runs_exactly,workspace_control_is_durable_scoped_and_projects_exact_stop_obligations,supervisor_stop_validates_every_fence_and_retries_an_orphaned_process
#[allow(clippy::too_many_lines)] // Root and recursive child obligations are reconciled in one ordered inventory pass.
pub(super) fn reconcile_supervisor_workers(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
    selected_run: Option<usagi_core::domain::supervisor::SupervisorRunId>,
    absence_is_final: bool,
) -> anyhow::Result<usize> {
    let (obligations, pending) = {
        let Ok(runtime) = supervisor.lock() else {
            anyhow::bail!("supervisor runtime is unavailable");
        };
        match selected_run {
            Some(id) => (
                runtime.worker_stop_obligations_for_run(id)?,
                runtime.pending_worker_stops_for_run(id)?,
            ),
            None => (
                runtime.worker_stop_obligations()?,
                runtime.pending_worker_stops()?,
            ),
        }
    };
    let Ok(mut runtime) = agent.lock() else {
        anyhow::bail!("agent owner is unavailable");
    };
    let mut first_failure = None;
    let mut pending_obligations = std::collections::BTreeMap::new();
    let mut acknowledged = Vec::new();
    for candidate in pending {
        let Some(worker) = runtime.runtime_for_operation(candidate.operation_id()) else {
            let operation_id = candidate.operation_id().to_string();
            let outcome = runtime.operation_outcome(&operation_id);
            if absence_is_final
                || matches!(
                outcome,
                Some(Err(error))
                    if error.code
                        != usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown
                )
            {
                // A durable definite admission failure proves that no worker
                // can still appear. Startup absence has the same meaning after
                // Agent hydration and before socket admission begins.
                acknowledged.push(candidate);
                continue;
            }
            // Goal admission may still be waiting for this Agent lock after
            // reserving its Supervisor root. Absence is therefore not proof
            // that no worker can appear; retain the stop fence for a later
            // reconciliation pass.
            continue;
        };
        if !candidate.matches_worker_scope(&worker) {
            // The operation is durably occupied by another scope, so this
            // Supervisor reservation can never acquire it. Never interrupt the
            // unrelated Agent.
            acknowledged.push(candidate);
            continue;
        }
        if let Some(expected) = candidate.worker_agent_id() {
            let actual = runtime
                .dispatch_store()
                .run(candidate.operation_id())?
                .map(|dispatch| dispatch.agent_id);
            let Some(actual) = actual else {
                first_failure.get_or_insert_with(|| {
                    anyhow::anyhow!("pending Supervisor worker Agent fence is unavailable")
                });
                continue;
            };
            if actual != expected {
                acknowledged.push(candidate);
                continue;
            }
        }
        if let Some(expected) = candidate.worker_profile_id() {
            let actual = match runtime.dispatch_store().run(candidate.operation_id())? {
                Some(dispatch) => runtime
                    .dispatch_store()
                    .agent(dispatch.agent_id)?
                    .map(|agent| agent.runtime),
                None => None,
            };
            let Some(actual) = actual else {
                first_failure.get_or_insert_with(|| {
                    anyhow::anyhow!("pending Supervisor worker runtime profile is unavailable")
                });
                continue;
            };
            if &actual != expected {
                acknowledged.push(candidate);
                continue;
            }
        }
        if let Some(expected) = candidate.worker_semantic_digest() {
            let actual = runtime
                .dispatch_store()
                .admission(candidate.operation_id())?
                .map(|admission| {
                    usagi_core::infrastructure::ipc::agent_operation_digest(&admission.semantic_key)
                });
            let Some(actual) = actual else {
                first_failure.get_or_insert_with(|| {
                    anyhow::anyhow!("pending Supervisor worker semantic fence is unavailable")
                });
                continue;
            };
            if actual != expected {
                acknowledged.push(candidate);
                continue;
            }
        }
        let (provenance, candidates) = pending_obligations
            .entry(candidate.workspace_id())
            .or_insert((
                Vec::<usagi_core::domain::supervisor::RunProvenance>::new(),
                Vec::new(),
            ));
        provenance.push(candidate.provenance(&worker)?);
        candidates.push(candidate);
    }
    let mut by_workspace = std::collections::BTreeMap::new();
    for (workspace, provenance) in obligations {
        by_workspace
            .entry(workspace)
            .or_insert(Vec::<usagi_core::domain::supervisor::RunProvenance>::new())
            .push(provenance);
    }
    let mut interrupted = 0;
    for (workspace, provenance) in by_workspace {
        match runtime.interrupt_supervisor_workers(workspace, &provenance) {
            Ok(count) => interrupted += count,
            Err(error) => {
                if first_failure.is_none() {
                    first_failure = Some(anyhow::anyhow!(error.message));
                }
            }
        }
    }
    for (workspace, (provenance, candidates)) in pending_obligations {
        match runtime.interrupt_supervisor_workers(workspace, &provenance) {
            Ok(count) => {
                interrupted += count;
                acknowledged.extend(candidates);
            }
            Err(error) => {
                if first_failure.is_none() {
                    first_failure = Some(anyhow::anyhow!(error.message));
                }
            }
        }
    }
    drop(runtime);
    if !acknowledged.is_empty() {
        let Ok(runtime) = supervisor.lock() else {
            anyhow::bail!("supervisor runtime is unavailable");
        };
        let result = runtime.acknowledge_pending_worker_stops(&acknowledged);
        if let Err(error) = result {
            first_failure.get_or_insert(error);
        }
    }
    match first_failure {
        Some(error) => Err(error),
        None => Ok(interrupted),
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=artifact_preparation_rejects_nonterminal_wrong_contract_and_corrupt_membership
pub(super) fn reconcile_pending_goal_artifacts(
    supervisor: &SharedSupervisorRuntime,
    workspaces: &Workspaces,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<usize> {
    let pending = supervisor
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor runtime is unavailable"))?
        .pending_artifact_verifications(now)?;
    let mut reconciled = 0;
    let mut first_failure = None;
    for item in pending {
        if let Err(error) = verify_completed_goal_artifact(
            supervisor,
            workspaces,
            Some(item.dispatch_run_id),
            GoalArtifactReport::Recovery,
        ) {
            first_failure.get_or_insert_with(|| {
                anyhow::anyhow!(
                    "artifact verification {} remains pending: {}",
                    item.dispatch_run_id,
                    error.message
                )
            });
        } else {
            reconciled += 1;
        }
    }
    first_failure.map_or(Ok(reconciled), Err)
}

pub(super) fn map_inbox_query_error(
    error: &anyhow::Error,
) -> usagi_core::infrastructure::ipc::ProtocolError {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    let message = error.to_string();
    if message.starts_with("dispatch inbox cursor")
        || message.starts_with("dispatch inbox ACK cursor")
        || message.starts_with("dispatch inbox page limit")
    {
        ProtocolError::new(ErrorCode::InvalidArgument, message)
    } else {
        ProtocolError::new(ErrorCode::Unavailable, "dispatch inbox is unavailable")
    }
}

#[derive(Clone)]
pub(super) struct AuthenticatedSupervisorCaller {
    pub(super) descriptor: String,
    pub(super) workspace: WorkspaceId,
    pub(super) dispatch_run_id: usagi_core::domain::id::OperationId,
    pub(super) runtime: AgentRuntimeRef,
}

#[allow(clippy::too_many_lines)]
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_supervisor_tools_observe_one_durable_aggregate
pub(super) fn dispatch_supervisor_tool(
    runtime: &SharedSupervisorRuntime,
    caller: Result<AuthenticatedSupervisorCaller, usagi_core::infrastructure::ipc::ProtocolError>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use chrono::Utc;
    use usagi_core::domain::{
        id::OperationId,
        supervisor::{EscalationDecision, SupervisorRunId, SupervisorRunState},
    };
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    #[derive(Deserialize)]
    struct StartPayload {
        root_task: String,
        #[serde(default)]
        initial_task_dag: Vec<InitialTask>,
        policy_selector: Option<String>,
    }
    #[derive(Deserialize)]
    struct RunPayload {
        supervisor_run_id: SupervisorRunId,
    }
    #[derive(Deserialize)]
    struct ListPayload {
        state: Option<SupervisorRunState>,
        caller: Option<String>,
        session: Option<String>,
        cursor: Option<String>,
        #[serde(default = "default_page_limit")]
        limit: usize,
    }
    #[derive(Deserialize)]
    struct CancelPayload {
        supervisor_run_id: SupervisorRunId,
        reason: String,
    }
    #[derive(Deserialize)]
    struct ResolvePayload {
        supervisor_run_id: SupervisorRunId,
        escalation_id: OperationId,
        decision: EscalationDecision,
    }
    #[derive(Deserialize)]
    struct EventsPayload {
        supervisor_run_id: SupervisorRunId,
        #[serde(default)]
        after_sequence: u64,
        #[serde(default = "default_page_limit")]
        limit: usize,
    }

    fn default_page_limit() -> usize {
        50
    }

    let parsed = serde_json::from_value::<DaemonRequest>(body.clone());
    let Ok(DaemonRequest::SupervisorTool {
        action,
        operation_id,
        payload,
        caller_context: _,
    }) = parsed
    else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let result = runtime
        .lock()
        .map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
        })
        .and_then(|runtime| {
            let authenticated = caller?;
            let caller = authenticated.descriptor;
            let workspace = authenticated.workspace;
            match action {
                SupervisorToolAction::Start => {
                    let input: StartPayload = serde_json::from_value(payload).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_start payload",
                        )
                    })?;
                    if !input.initial_task_dag.is_empty() {
                        return Err(ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "supervisor_start accepts one live root; delegate child work through session tools",
                        ));
                    }
                    runtime
                        .ensure_supervisor_start_dispatch_available(
                            &operation_id,
                            authenticated.dispatch_run_id,
                        )
                        .map_err(supervisor_error)?;
                    runtime
                        .start_for_workspace_caller_dispatch(
                            &caller,
                            workspace,
                            &operation_id,
                            input.root_task,
                            input.policy_selector,
                            authenticated.dispatch_run_id,
                            &authenticated.runtime,
                            Utc::now(),
                        )
                        .map_err(supervisor_error)?;
                    let started = runtime
                        .bind_reserved_caller_dispatch(
                            &operation_id,
                            authenticated.dispatch_run_id,
                            &authenticated.runtime,
                            Utc::now(),
                        )
                        .map_err(supervisor_error)?;
                    runtime
                        .tick(
                            started.supervisor_run_id,
                            Utc::now(),
                            &mut DeferredDecisionWaker,
                        )
                        .map_err(supervisor_error)?;
                    serde_json::to_value(
                        runtime
                            .get(&caller, started.supervisor_run_id)
                            .map_err(supervisor_error)?
                            .ok_or_else(|| {
                                ProtocolError::new(
                                    ErrorCode::Internal,
                                    "started supervisor run disappeared",
                                )
                            })?,
                    )
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
                    })
                }
                SupervisorToolAction::Get => {
                    let input: RunPayload = serde_json::from_value(payload).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_get payload",
                        )
                    })?;
                    let value = serde_json::to_value(
                        runtime
                            .get(&caller, input.supervisor_run_id)
                            .map_err(supervisor_error)?
                            .ok_or_else(|| {
                                ProtocolError::new(
                                    ErrorCode::OwnershipUnknown,
                                    "supervisor run is unavailable to this caller",
                                )
                            })?,
                    )
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
                    })?;
                    bounded_supervisor_query(value).map_err(supervisor_error)
                }
                SupervisorToolAction::List => {
                    let input: ListPayload = serde_json::from_value(payload).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_list payload",
                        )
                    })?;
                    if input.limit == 0
                        || input.limit > 100
                        || input.session.is_some()
                        || input.caller.as_ref().is_some_and(|value| value != &caller)
                    {
                        return Err(ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_list filter",
                        ));
                    }
                    let offset = input
                        .cursor
                        .as_deref()
                        .unwrap_or("0")
                        .parse::<usize>()
                        .map_err(|_| {
                            ProtocolError::new(
                                ErrorCode::InvalidArgument,
                                "invalid supervisor_list cursor",
                            )
                        })?;
                    let value = serde_json::to_value(
                        runtime
                            .list_page(&caller, input.state, offset, input.limit)
                            .map_err(supervisor_error)?,
                    )
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
                    })?;
                    bounded_supervisor_query(value).map_err(supervisor_error)
                }
                SupervisorToolAction::Cancel => {
                    let input: CancelPayload = serde_json::from_value(payload).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_cancel payload",
                        )
                    })?;
                    serde_json::to_value(
                        runtime
                            .cancel(&caller, input.supervisor_run_id, input.reason, Utc::now())
                            .map_err(supervisor_error)?,
                    )
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
                    })
                }
                SupervisorToolAction::ResolveEscalation => {
                    let input: ResolvePayload = serde_json::from_value(payload).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_resolve_escalation payload",
                        )
                    })?;
                    serde_json::to_value(
                        runtime
                            .resolve_escalation(
                                &caller,
                                input.supervisor_run_id,
                                input.escalation_id,
                                input.decision,
                                Utc::now(),
                            )
                            .map_err(supervisor_error)?,
                    )
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
                    })
                }
                SupervisorToolAction::Events => {
                    let input: EventsPayload = serde_json::from_value(payload).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_events payload",
                        )
                    })?;
                    if input.limit == 0 || input.limit > 100 {
                        return Err(ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_events limit",
                        ));
                    }
                    let (events, cursor) = runtime
                        .events(
                            &caller,
                            input.supervisor_run_id,
                            input.after_sequence,
                            input.limit,
                        )
                        .map_err(supervisor_error)?;
                    bounded_supervisor_query(
                        serde_json::json!({"events": events, "next_sequence": cursor.next_sequence}),
                    )
                    .map_err(supervisor_error)
                }
            }
        });
    match result {
        Ok(value) => envelope(hello, request_id, ResponseOutcome::Ok, value),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-08-31 tests=supervisor_snapshot_is_exactly_workspace_scoped
pub(super) fn connection_workspace_id(
    bound: &ConnectionWorkspace,
) -> Result<WorkspaceId, usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    let Ok(sessions) = bound.sessions().lock() else {
        return Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "session runtime is unavailable",
        ));
    };
    let Ok(snapshot) = sessions.snapshot() else {
        return Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "workspace identity is unavailable",
        ));
    };
    let Some(value) = snapshot.get("workspace_id") else {
        return Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "workspace identity is unavailable",
        ));
    };
    match serde_json::from_value(value.clone()) {
        Ok(workspace) => Ok(workspace),
        Err(_) => Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "workspace identity is unavailable",
        )),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=supervisor_snapshot_is_exactly_workspace_scoped
pub(super) fn dispatch_supervisor_snapshot(
    runtime: &SharedSupervisorRuntime,
    bound: &ConnectionWorkspace,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot;
    use usagi_core::infrastructure::client::DaemonRequest;
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let result = (|| {
        let Ok(DaemonRequest::SupervisorSnapshot {
            workspace: requested,
        }) = serde_json::from_value::<DaemonRequest>(body.clone())
        else {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid supervisor snapshot request",
            ));
        };
        let workspace = connection_workspace_id(bound)?;
        if requested != workspace {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "supervisor snapshot belongs to another workspace",
            ));
        }
        let mut runs = runtime
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
            })?
            .list_workspace(workspace)
            .map_err(supervisor_error)?;
        // The TUI draws current state only. Event provenance has its own
        // capability-scoped MCP surface and is omitted here to keep the
        // periodic projection small even for long-lived runs.
        for run in &mut runs {
            run.provenance.clear();
        }
        let mut snapshot = SupervisorWorkspaceSnapshot {
            workspace_id: workspace,
            runs,
        };
        loop {
            let value = serde_json::to_value(&snapshot).map_err(|_| {
                ProtocolError::new(ErrorCode::Internal, "supervisor snapshot encoding failed")
            })?;
            match bounded_supervisor_query(value) {
                Ok(value) => return Ok(value),
                Err(_) if !snapshot.runs.is_empty() => {
                    snapshot.runs.pop();
                }
                Err(error) => return Err(supervisor_error(error)),
            }
        }
    })();
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

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=workspace_control_is_durable_scoped_and_projects_exact_stop_obligations,supervisor_delete_dispatch_returns_exact_receipt,supervisor_stop_validates_every_fence_and_retries_an_orphaned_process
pub(super) fn dispatch_supervisor_control(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use chrono::Utc;
    use usagi_core::domain::supervisor::SupervisorRunState;
    use usagi_core::infrastructure::client::DaemonRequest;
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let result = (|| {
        let Ok(DaemonRequest::SupervisorControl {
            workspace: requested,
            operation_id,
            command,
        }) = serde_json::from_value::<DaemonRequest>(body.clone())
        else {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid supervisor control request",
            ));
        };
        let workspace = connection_workspace_id(bound)?;
        if requested != workspace {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "supervisor control belongs to another workspace",
            ));
        }

        if matches!(
            &command,
            usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Delete { .. }
        ) {
            let deleted = supervisor
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
                })?
                .delete_for_workspace(workspace, operation_id, &command, Utc::now())
                .map_err(supervisor_control_error)?;
            let value = serde_json::to_value(deleted).map_err(|_| {
                supervisor_control_unconfirmed("supervisor deletion response encoding failed")
            })?;
            return bounded_supervisor_query(value).map_err(|_| {
                supervisor_control_unconfirmed("supervisor deletion response is unavailable")
            });
        }

        prompt_supervisor_retry(supervisor, agent, workspace, &command)?;

        let mut run = supervisor
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
            })?
            .control_for_workspace(workspace, operation_id, &command, Utc::now())
            .map_err(supervisor_control_error)?;

        if matches!(
            run.state,
            SupervisorRunState::Cancelled | SupervisorRunState::Failed
        ) {
            reconcile_supervisor_run_workers(supervisor, agent, run.supervisor_run_id).map_err(
                |_| {
                    supervisor_control_unconfirmed("supervisor workers could not be stopped safely")
                },
            )?;
        } else if run.state == SupervisorRunState::Running {
            let runtime = supervisor
                .lock()
                .map_err(|_| supervisor_control_unconfirmed("supervisor runtime is unavailable"))?;
            runtime
                .tick(
                    run.supervisor_run_id,
                    Utc::now(),
                    &mut AgentDecisionWaker { agent },
                )
                .map_err(|_| {
                    supervisor_control_unconfirmed("supervisor retry could not advance")
                })?;
            run = runtime
                .get_for_workspace(workspace, run.supervisor_run_id)
                .map_err(|_| {
                    supervisor_control_unconfirmed("supervisor control result is unavailable")
                })?
                .ok_or_else(|| {
                    supervisor_control_unconfirmed(
                        "supervisor control result is unavailable to this workspace",
                    )
                })?;
        }

        // Match the read-only TUI projection: worker/session/worktree
        // provenance is an internal control input, not human UI response data.
        run.provenance.clear();
        let value = serde_json::to_value(run)
            .map_err(|_| supervisor_control_unconfirmed("supervisor response encoding failed"))?;
        bounded_supervisor_query(value)
            .map_err(|_| supervisor_control_unconfirmed("supervisor response is unavailable"))
    })();
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

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=generic_escalation_resume_does_not_require_an_agent_prompt,workspace_control_is_durable_scoped_and_projects_exact_stop_obligations
pub(super) fn prompt_supervisor_retry(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
    workspace: WorkspaceId,
    command: &usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::domain::supervisor::{EscalationDecision, SupervisorWorkspaceCommand};

    let SupervisorWorkspaceCommand::ResolveEscalation {
        supervisor_run_id,
        escalation_id,
        decision: EscalationDecision::Resume,
    } = command
    else {
        return Ok(());
    };
    let Some(retry) = supervisor
        .lock()
        .map_err(|_| supervisor_control_unconfirmed("supervisor runtime is unavailable"))?
        .retry_work_for_workspace(workspace, *supervisor_run_id, *escalation_id)
        .map_err(supervisor_control_error)?
    else {
        return Ok(());
    };
    let prompt = format!(
        "The user requested a Supervisor retry. Continue the blocked work, correct the problem, and report completion again before stopping. Reason: {}. Evidence: {}",
        retry.reason, retry.safe_evidence
    );
    agent
        .lock()
        .map_err(|_| supervisor_control_unconfirmed("retry Agent owner is unavailable"))?
        .prompt_run(retry.provenance.dispatch_run_id, &prompt)
        .map_err(|_| {
            supervisor_control_unconfirmed(
                "retry Agent is no longer live; resume its conversation first",
            )
        })?;
    Ok(())
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_supervisor_tools_observe_one_durable_aggregate
pub(super) fn authenticated_supervisor_caller(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    client: &usagi_core::domain::id::ClientId,
    body: &serde_json::Value,
) -> Result<AuthenticatedSupervisorCaller, usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    let credential = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::SupervisorTool { caller_context, .. } => caller_context,
            _ => None,
        })
        .filter(|context| !context.credential.is_empty())
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "supervisor caller provenance is unknown",
            )
        })?;
    let workspace = bound
        .sessions()
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable"))?
        .snapshot()
        .map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "workspace identity is unavailable")
        })?
        .get("workspace_id")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(|| {
            ProtocolError::new(ErrorCode::Unavailable, "workspace identity is unavailable")
        })?;
    let authenticated = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
        .mcp_dispatch_context(&credential.credential)
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "supervisor caller provenance is unknown",
            )
        })?;
    if authenticated.workspace_id != workspace {
        return Err(ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "supervisor caller does not belong to this workspace",
        ));
    }
    Ok(AuthenticatedSupervisorCaller {
        descriptor: supervisor_caller_descriptor(client, &authenticated.caller),
        workspace,
        dispatch_run_id: authenticated.run_id,
        runtime: authenticated.runtime,
    })
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_supervisor_tools_observe_one_durable_aggregate
pub(super) fn supervisor_caller_descriptor(
    client: &usagi_core::domain::id::ClientId,
    caller: &usagi_core::domain::agent::CallerRef,
) -> String {
    let session = caller
        .session_id
        .map_or_else(|| "root".to_owned(), |session| session.to_string());
    format!(
        "ipc-client:{};session:{session};agent:{}",
        client, caller.agent_id
    )
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_supervisor_tools_observe_one_durable_aggregate
pub(super) fn supervisor_error(
    error: anyhow::Error,
) -> usagi_core::infrastructure::ipc::ProtocolError {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let message = error.to_string();
    drop(error);
    let code = if message.contains("capacity is exhausted")
        || message.contains("supervisor policy denied delegated dispatch")
    {
        ErrorCode::ResourceExhausted
    } else if message.contains("already belongs to another retained supervisor run")
        || message.contains("stale supervisor ownership")
        || message.contains("conflicting retained supervisor ownership")
        || message.contains("closed supervisor ownership")
    {
        ErrorCode::RevisionConflict
    } else if message.contains("reused")
        || message.contains("conflicts with its reservation")
        || message.contains("conflicts with its existing supervisor task")
        || message.contains("delegated dispatch operation is already in use")
        || message.contains("delegated dispatch operation already owns a supervisor task")
    {
        ErrorCode::IdempotencyConflict
    } else if message.contains("does not exist") || message.contains("does not belong") {
        ErrorCode::OwnershipUnknown
    } else if message.contains("failed to")
        || message.contains("is unavailable")
        || message.contains("stale supervisor state revision")
    {
        ErrorCode::Unavailable
    } else {
        ErrorCode::InvalidArgument
    };
    ProtocolError::new(code, message)
}

pub(super) fn require_stable_supervisor_fence<T: PartialEq>(
    supervision_at_preflight: Option<&T>,
    supervision_before_reservation: Option<&T>,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    if supervision_at_preflight != supervision_before_reservation {
        return Err(usagi_core::infrastructure::ipc::ProtocolError::new(
            usagi_core::infrastructure::ipc::ErrorCode::RevisionConflict,
            "Supervisor ownership fence changed before delegated dispatch reservation",
        ));
    }
    Ok(())
}

pub(super) fn require_supervisor_reservation_presence<T>(
    supervision_at_preflight: Option<&T>,
    reservation_present: bool,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    if supervision_at_preflight.is_some() != reservation_present {
        return Err(usagi_core::infrastructure::ipc::ProtocolError::new(
            usagi_core::infrastructure::ipc::ErrorCode::RevisionConflict,
            "Supervisor reservation disagrees with its ownership fence",
        ));
    }
    Ok(())
}

pub(super) fn supervisor_control_error(
    error: anyhow::Error,
) -> usagi_core::infrastructure::ipc::ProtocolError {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let message = error.to_string();
    drop(error);
    if message.contains("capacity is exhausted") {
        ProtocolError::new(
            ErrorCode::ResourceExhausted,
            "supervisor control capacity is exhausted",
        )
    } else if message.contains("conflicts with its reservation")
        || message.contains("conflicts with its semantic payload")
    {
        ProtocolError::new(
            ErrorCode::IdempotencyConflict,
            "supervisor control operation was reused for another command",
        )
    } else if message.contains("outside the retained") {
        ProtocolError::new(
            ErrorCode::IdempotencyExpired,
            "supervisor control operation is outside the retained replay window",
        )
    } else if message.contains("does not belong") {
        ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "supervisor run is unavailable to this workspace",
        )
    } else if message.starts_with("invalid supervisor cancellation reason")
        || matches!(
            message.as_str(),
            "InvalidTransition" | "ProvenanceMismatch" | "TerminalRun"
        )
    {
        ProtocolError::new(
            ErrorCode::InvalidArgument,
            "supervisor control command is invalid or stale",
        )
    } else {
        supervisor_control_unconfirmed("supervisor control could not be persisted")
    }
}

pub(super) fn supervisor_control_unconfirmed(
    message: &str,
) -> usagi_core::infrastructure::ipc::ProtocolError {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, RetryMode, SideEffect};
    let mut error = ProtocolError::new(ErrorCode::Unavailable, message);
    error.retry_mode = RetryMode::SameOperation;
    error.side_effect = SideEffect::PartialOrUnknown;
    error
}

/// PR events are deliberately only hints; the IPC request always returns this
/// durable snapshot so reconnects and dropped events converge without replay.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=pr_snapshot_events_cover_success_scoped_and_lane_errors
pub(super) fn dispatch_pr_snapshot(
    inventory: &SharedPrInventory,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::client::{DaemonRequest, PrAction};
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    let result = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Pr {
                action: PrAction::Snapshot,
                payload,
            } => inventory
                .lock()
                .ok()
                .and_then(|mut projector| projector.snapshot(payload.session_id).ok())
                .and_then(|snapshot| serde_json::to_value(snapshot).ok()),
            DaemonRequest::PrBatch { payload } => inventory
                .lock()
                .ok()
                .and_then(|mut projector| projector.snapshots(&payload.session_ids).ok())
                .and_then(|snapshots| serde_json::to_value(snapshots).ok()),
            DaemonRequest::PrDismiss { payload } => inventory
                .lock()
                .ok()
                .and_then(|mut projector| {
                    projector.dismiss(payload.session_id, &payload.url).ok()?;
                    projector.snapshot(payload.session_id).ok()
                })
                .and_then(|snapshot| serde_json::to_value(snapshot).ok()),
            _ => None,
        });
    let (outcome, body) = result.map_or_else(
        || {
            (
                ResponseOutcome::Error(ProtocolError::new(
                    ErrorCode::InvalidArgument,
                    "invalid PR snapshot request",
                )),
                serde_json::json!(null),
            )
        },
        |snapshot| (ResponseOutcome::Ok, snapshot),
    );
    usagi_core::infrastructure::ipc::Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: usagi_core::infrastructure::ipc::EnvelopeKind::Response {
            request_id,
            outcome,
            body,
        },
    }
}

/// Handles the decision subset of the MCP dispatch registry.  The MCP payload
/// never carries an owner: it is reconstructed from the one active durable
/// dispatch binding.  Ambiguity is deliberately fail-closed, preventing an
/// agent from choosing another workspace, caller, or run.
#[derive(Debug)]
pub(super) enum UserDecisionDispatchError {
    Decision(usagi_core::domain::user_decision::UserDecisionError),
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_user_decision_round_trip_reaches_the_original_caller
impl From<usagi_core::domain::user_decision::UserDecisionError> for UserDecisionDispatchError {
    fn from(error: usagi_core::domain::user_decision::UserDecisionError) -> Self {
        Self::Decision(error)
    }
}

#[allow(clippy::too_many_lines)] // The complete wire-to-store error mapping is one atomic routing contract.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_user_decision_round_trip_reaches_the_original_caller
pub(super) fn dispatch_user_decision(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    store: &UserDecisionStore,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use chrono::Utc;
    use usagi_core::domain::agent::RunStatus;
    use usagi_core::domain::id::UserDecisionId;
    use usagi_core::domain::user_decision::{
        UserDecision, UserDecisionAnswer, UserDecisionError, UserDecisionOwner, UserDecisionStatus,
    };
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    #[derive(Deserialize)]
    struct RequestPayload {
        title: String,
        prompt: String,
        options: Vec<usagi_core::domain::user_decision::UserDecisionOption>,
        #[serde(default)]
        allow_freeform: bool,
        #[serde(default)]
        expires_at: Option<chrono::DateTime<Utc>>,
        #[serde(default)]
        idempotency_key: Option<String>,
    }
    #[derive(Deserialize)]
    struct DecisionIdPayload {
        decision_id: UserDecisionId,
    }
    #[derive(Deserialize)]
    struct ResolvePayload {
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
    }

    let parsed = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::DispatchTool {
                action,
                payload,
                caller_context,
                ..
            } => Some((action, payload, caller_context, false)),
            DaemonRequest::UserDecision { action, payload } => {
                use usagi_core::infrastructure::client::TuiUserDecisionAction;
                let action = match action {
                    TuiUserDecisionAction::Get => DispatchToolAction::UserDecisionGet,
                    TuiUserDecisionAction::List => DispatchToolAction::UserDecisionList,
                    TuiUserDecisionAction::Resolve => DispatchToolAction::UserDecisionResolve,
                    TuiUserDecisionAction::Cancel => DispatchToolAction::UserDecisionCancel,
                };
                Some((action, payload, None, true))
            }
            _ => None,
        });
    let Some((action, payload, caller_context, tui_access)) = parsed else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    if !matches!(
        action,
        DispatchToolAction::UserDecisionRequest
            | DispatchToolAction::UserDecisionGet
            | DispatchToolAction::UserDecisionList
            | DispatchToolAction::UserDecisionResolve
            | DispatchToolAction::UserDecisionCancel
            | DispatchToolAction::UserDecisionExpire
    ) {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    }

    let workspace = (|| -> Result<_, ProtocolError> {
        bound
            .sessions()
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
            })?
            .snapshot()
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::Unavailable,
                    "daemon could not read managed sessions",
                )
            })?
            .get("workspace_id")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::Unavailable, "workspace identity is unavailable")
            })
    })();
    let owner = workspace.and_then(|workspace| -> Result<_, ProtocolError> {
        if tui_access {
            return Ok((workspace, None));
        }
        let runtime = agent.lock().map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
        })?;
        let credential = caller_context.as_ref().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "decision caller provenance is unknown",
            )
        })?;
        let authenticated = runtime
            .mcp_dispatch_context(&credential.credential)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "decision caller provenance is unknown",
                )
            })?;
        if authenticated.workspace_id != workspace {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "decision caller does not belong to this workspace",
            ));
        }
        let run_id = authenticated.run_id;
        let dispatch = runtime.dispatch_store();
        let run = dispatch
            .runs()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "dispatch provenance is unavailable")
            })?
            .into_iter()
            .find(|run| run.run_id == run_id && run.status == RunStatus::Running)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "decision caller provenance is unknown",
                )
            })?;
        let binding = dispatch
            .binding(run_id)
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "dispatch provenance is unavailable")
            })?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "decision caller provenance is unavailable",
                )
            })?;
        if binding.worker.agent_id != run.agent_id {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "decision caller provenance is inconsistent",
            ));
        }
        Ok((
            workspace,
            Some(UserDecisionOwner {
                workspace_id: workspace,
                session_id: binding.worker.session_id,
                caller: binding.caller,
                run_id,
            }),
        ))
    });
    let response = owner.and_then(|(workspace, owner)| {
        let request_owner = owner.clone();
        let decision_for = |id| -> Result<UserDecision, UserDecisionError> {
            let decision = store
                .get(workspace, id)
                .map_err(|_| UserDecisionError::Terminal)?
                .ok_or(UserDecisionError::Terminal)?;
            if request_owner
                .as_ref()
                .is_some_and(|expected| decision.owner != *expected)
            {
                return Err(UserDecisionError::Terminal);
            }
            Ok(decision)
        };
        let now = Utc::now();
        let result = (|| -> Result<serde_json::Value, UserDecisionDispatchError> {
            match action {
                DispatchToolAction::UserDecisionRequest => {
                    let owner = owner.ok_or(UserDecisionError::Terminal)?;
                    let input = serde_json::from_value::<RequestPayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let decision = store
                        .create(UserDecision {
                            decision_id: UserDecisionId::new(),
                            owner,
                            title: input.title,
                            prompt: input.prompt,
                            options: input.options,
                            allow_freeform: input.allow_freeform,
                            // An omitted deadline is finite by default so an
                            // abandoned synchronous waiter cannot occupy a
                            // pending slot forever.
                            expires_at: input
                                .expires_at
                                .or_else(|| now.checked_add_signed(chrono::Duration::hours(24))),
                            idempotency_key: input.idempotency_key,
                            status: UserDecisionStatus::Pending,
                            answer: None,
                            created_at: now,
                            resolved_at: None,
                        })
                        .map_err(|_| UserDecisionError::Terminal)??;
                    Ok(serde_json::json!(decision))
                }
                DispatchToolAction::UserDecisionGet => {
                    let input = serde_json::from_value::<DecisionIdPayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let decision = decision_for(input.decision_id)?;
                    if decision.status != UserDecisionStatus::Pending {
                        // Resolution creates a durable outbox entry atomically.
                        // Polling the terminal decision is the acknowledgement;
                        // no synchronous connection is kept open while a human
                        // considers the answer.
                        store
                            .ack_event(input.decision_id)
                            .map_err(|_| UserDecisionError::Terminal)?;
                    }
                    Ok(serde_json::json!(decision))
                }
                DispatchToolAction::UserDecisionList => {
                    let decisions = store
                        .pending(workspace)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let decisions = decisions
                        .into_iter()
                        .filter(|decision| {
                            owner
                                .as_ref()
                                .is_none_or(|expected| decision.owner == *expected)
                        })
                        .collect::<Vec<_>>();
                    Ok(serde_json::json!({"workspace": workspace, "decisions": decisions}))
                }
                DispatchToolAction::UserDecisionResolve => {
                    let input = serde_json::from_value::<ResolvePayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let _ = decision_for(input.decision_id)?;
                    let decision = store
                        .resolve(workspace, input.decision_id, input.answer, now)
                        .map_err(|_| UserDecisionError::Terminal)??;
                    Ok(serde_json::json!(decision))
                }
                DispatchToolAction::UserDecisionCancel | DispatchToolAction::UserDecisionExpire => {
                    let input = serde_json::from_value::<DecisionIdPayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let _ = decision_for(input.decision_id)?;
                    let status = if action == DispatchToolAction::UserDecisionCancel {
                        UserDecisionStatus::Cancelled
                    } else {
                        UserDecisionStatus::Expired
                    };
                    let decision = store
                        .terminal(workspace, input.decision_id, status, now)
                        .map_err(|_| UserDecisionError::Terminal)??;
                    Ok(serde_json::json!(decision))
                }
                _ => unreachable!(),
            }
        })();
        let value = result.map_err(|error| {
            let (code, message) = match error {
                UserDecisionDispatchError::Decision(UserDecisionError::IdempotencyConflict) => (
                    ErrorCode::IdempotencyConflict,
                    "decision idempotency key conflicts",
                ),
                UserDecisionDispatchError::Decision(UserDecisionError::IdempotencyExpired) => (
                    ErrorCode::IdempotencyExpired,
                    "decision idempotency result is no longer retained; use a new key for a new request",
                ),
                UserDecisionDispatchError::Decision(UserDecisionError::InvalidRequest) => (
                    ErrorCode::InvalidArgument,
                    "decision request must be bounded and offer at least one answer path",
                ),
                UserDecisionDispatchError::Decision(UserDecisionError::InvalidOption) => {
                    (ErrorCode::InvalidArgument, "decision option is not allowed")
                }
                UserDecisionDispatchError::Decision(UserDecisionError::FreeformNotAllowed) => (
                    ErrorCode::InvalidArgument,
                    "freeform decision answer is not allowed",
                ),
                UserDecisionDispatchError::Decision(UserDecisionError::Expired) => {
                    (ErrorCode::DeadlineExceeded, "decision has expired")
                }
                UserDecisionDispatchError::Decision(UserDecisionError::Terminal) => (
                    ErrorCode::RevisionConflict,
                    "decision is not pending or is outside this workspace",
                ),
                // Nothing was written, so retrying after the backlog clears is
                // safe and is the intended response.
                UserDecisionDispatchError::Decision(UserDecisionError::PendingLimitReached) => (
                    ErrorCode::ResourceExhausted,
                    "the unanswered decision backlog is full for this workspace or daemon; \
                     answer some before asking another",
                ),
                UserDecisionDispatchError::Decision(UserDecisionError::CapacityReached) => (
                    ErrorCode::ResourceExhausted,
                    "the user decision store is full of pending or undelivered records; \
                     complete some before retrying",
                ),
            };
            ProtocolError::new(code, message)
        })?;
        Ok(value)
    });
    match response {
        Ok(value) => envelope(hello, request_id, ResponseOutcome::Ok, value),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
pub(super) fn dispatch_dispatch(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::client::{DaemonRequest, SessionAction};
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    let Some((operation_id, intent)) = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Dispatch {
                operation_id,
                intent,
            } => Some((operation_id, intent)),
            _ => None,
        })
    else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let session_id = (|| {
        let mut runtime = bound.sessions().lock().map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
        })?;
        let snapshot = runtime.snapshot().map_err(|_| {
            ProtocolError::new(
                ErrorCode::Unavailable,
                "daemon could not read managed sessions",
            )
        })?;
        if let Some(id) = session_id_by_name(&snapshot, &intent.session_name) {
            return Ok(id);
        }
        let created = runtime
            .handle(
                SessionAction::Create,
                &operation_id,
                &serde_json::json!({"name": intent.session_name}),
            )
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InvalidArgument, error.safe_message())
            })?;
        session_id_by_name(&created.body, &intent.session_name).ok_or_else(|| {
            ProtocolError::new(ErrorCode::Unavailable, "created session is not available")
        })
    })();
    let result = session_id.and_then(|session_id| {
        let scope = bound.scope_resolver();
        dispatch_agent_after_preflight(agent, &operation_id, &intent, session_id, &scope, None)
    });
    match result {
        Ok(admission) => envelope(
            hello,
            request_id,
            ResponseOutcome::Accepted {
                operation_id: usagi_core::infrastructure::ipc::OperationId(
                    admission.operation_id.clone(),
                ),
                operation_revision: admission.revision,
            },
            serde_json::json!({"run_id": admission.operation_id, "terminal": admission.terminal, "completed": admission.completed}),
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_brief_immediately_dispatches_an_isolated_triage_worker
pub(super) fn session_id_by_name(snapshot: &serde_json::Value, name: &str) -> Option<SessionId> {
    session_lineage_by_name(snapshot, name).map(|(session_id, _)| session_id)
}

pub(super) fn session_lineage_by_name(
    snapshot: &serde_json::Value,
    name: &str,
) -> Option<(SessionId, Option<SessionId>)> {
    let session = snapshot
        .get("sessions")?
        .as_array()?
        .iter()
        .find(|session| {
            session.get("name").and_then(serde_json::Value::as_str) == Some(name)
                && session.get("lifecycle").and_then(serde_json::Value::as_str) == Some("available")
        })?;
    let session_id = serde_json::from_value(session.get("session_id")?.clone()).ok()?;
    let parent_session_id = session
        .get("parent_session_id")
        .filter(|parent| !parent.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .ok()?;
    Some((session_id, parent_session_id))
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_agent_session_tools_only_reach_sessions_created_by_the_caller
pub(super) fn record_session_lineage(
    agent: &SharedAgentRuntime,
    workspace_id: WorkspaceId,
    snapshot: &serde_json::Value,
    name: &str,
) -> Result<SessionId, SessionRuntimeError> {
    let (session_id, parent_session_id) =
        session_lineage_by_name(snapshot, name).ok_or(SessionRuntimeError::Storage)?;
    agent
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .dispatch_store()
        .record_session_parent(workspace_id, session_id, parent_session_id)
        .map_err(|_| SessionRuntimeError::Storage)?;
    Ok(session_id)
}

#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
#[allow(clippy::too_many_lines)] // One guarded stop, handoff, and pre-commit recovery transaction.
pub(super) fn dispatch_rollover(
    data_dir: &Path,
    fence: &GenerationFence,
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Rollover {
                operation_id,
                restart_agents,
            } => Some((OperationId(operation_id), restart_agents)),
            _ => None,
        });
    let Some((operation, restart_agents)) = request else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let registry = match GenerationRegistryFile::new(data_dir) {
        Ok(file) => GenerationRegistry::new(file, DEFAULT_GENERATION_LIMIT),
        Err(error) => {
            return envelope(
                hello,
                request_id,
                ResponseOutcome::Error(ProtocolError::new(ErrorCode::Busy, error.to_string())),
                serde_json::Value::Null,
            );
        }
    };
    let mut pending_restart = None;
    let result = rollover_trigger::execute_with_guard(
        &registry,
        &CurrentLocatorFile::new(data_dir),
        &fence.gate,
        &fence.ledger,
        &UnixStandbyProbe {
            data_dir,
            build: current_build(),
        },
        &operation,
        &mut || {
            let mut agent = agent.lock().map_err(|_| {
                usagi_daemon::usecase::authority::routing::RolloverRefusal::McpAuthorityUnavailable
            })?;
            if let Some(restart) = &restart_agents {
                let planned = agent
                    .plan_daemon_restart_agents(&restart.expected, restart.force)
                    .map_err(|error| {
                        usagi_daemon::usecase::authority::routing::RolloverRefusal::AgentRestartRefused {
                            reason: error.message,
                        }
                    })?;
                let workspace_root = planned
                    .agents
                    .first()
                    .and_then(|item| {
                        bound
                            .workspaces
                            .workspace(item.runtime.terminal.workspace_id)
                    })
                    .map(|tenant| tenant.root().to_path_buf());
                if !planned.agents.is_empty() && workspace_root.is_none() {
                    return Err(
                        usagi_daemon::usecase::authority::routing::RolloverRefusal::AgentRestartRefused {
                            reason: "live Agent workspace is no longer adopted; no Agent was stopped"
                                .to_owned(),
                        },
                    );
                }
                if let Some(workspace_root) = workspace_root {
                    let pending = PendingDaemonAgentRestart::new(
                        &operation,
                        fence.gate.generation(),
                        workspace_root,
                        &planned,
                    );
                    pending_restart = Some(pending.clone());
                    // Persist before the first signal. A process crash can now
                    // leave extra still-live entries, never an unrecorded
                    // stopped source; recovery skips those exact live entries.
                    write_pending_daemon_agent_restart(data_dir, &pending).map_err(|error| {
                        usagi_daemon::usecase::authority::routing::RolloverRefusal::AgentRestartRefused {
                            reason: format!("could not persist Agent restart recovery: {error}"),
                        }
                    })?;
                }
                match agent.interrupt_agents_for_daemon_restart(
                    &restart.expected,
                    &restart.runtimes,
                    restart.force,
                ) {
                    Ok(_) => {}
                    Err(failure) => {
                        return Err(
                            usagi_daemon::usecase::authority::routing::RolloverRefusal::AgentRestartRefused {
                                reason: failure.error.message,
                            },
                        );
                    }
                }
            }
            let credentials = agent.provisioned_mcp_callers();
            if credentials == 0 {
                Ok(())
            } else {
                Err(
                    usagi_daemon::usecase::authority::routing::RolloverRefusal::McpAuthorityRetained {
                        credentials,
                    },
                )
            }
        },
    );
    if result.is_err()
        && let Some(pending) = &pending_restart
    {
        // A failed call may have written W1. Resolve that durable boundary
        // before minting old-owner credentials: if recovery rolls forward, the
        // successor owns the transaction; if it aborts, this owner may resume.
        if registry.load().is_ok_and(|snapshot| {
            snapshot
                .document()
                .handoff
                .as_ref()
                .is_some_and(|handoff| handoff.operation == operation)
        }) {
            let _ = recover_rollover(
                &registry,
                &CurrentLocatorFile::new(data_dir),
                &mut observe_generation_process,
            );
        }
        let old_still_active = registry.load().is_ok_and(|snapshot| {
            snapshot.document().current == Some(fence.gate.generation())
                && !snapshot
                    .document()
                    .handoff
                    .as_ref()
                    .is_some_and(|handoff| handoff.operation == operation)
        });
        if old_still_active {
            // The old owner performed the stop, so it also owns effect-zero
            // recovery if the handoff failed before W2. The durable transaction and
            // its background worker are the second safety net when this request or
            // its client disappears.
            let workspace_id = pending.agents[0].agent.target.workspace_id;
            let mut rollback = pending.clone();
            let restored = bound
                .workspaces
                .workspace(workspace_id)
                .ok_or_else(|| std::io::Error::other("Agent workspace is unavailable"))
                .and_then(|tenant| {
                    let recovery_bound = ConnectionWorkspace {
                        tenant,
                        workspaces: Arc::clone(&bound.workspaces),
                    };
                    restore_pending_daemon_agents(
                        data_dir,
                        agent,
                        &recovery_bound.scope_resolver(),
                        &mut rollback,
                        false,
                    )
                });
            if let Err(error) = restored {
                ErrorLog::record(&format!(
                    "daemon rollover pre-commit Agent rollback failed: {error}"
                ));
            } else if let Err(error) =
                clear_pending_daemon_agent_restart(data_dir, &pending.operation_id)
            {
                ErrorLog::record(&format!(
                    "daemon rollover Agent rollback marker cleanup deferred: {error}"
                ));
            }
        } else {
            // The successor worker owns post-commit recovery.
        }
    }
    let result = result.map_err(|error| error.to_string());
    match result {
        Ok(outcome) => envelope(
            hello,
            request_id,
            ResponseOutcome::Accepted {
                operation_id: operation,
                operation_revision: 1,
            },
            serde_json::json!({"outcome": format!("{outcome:?}")}),
        ),
        Err(message) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(ProtocolError::new(ErrorCode::Busy, message)),
            serde_json::Value::Null,
        ),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=metrics_snapshot_is_served_through_the_daemon_endpoint
pub(super) fn dispatch_metrics(
    metrics: &SharedMetricsBroker,
    process_metrics: &SharedProcessResourceSampler,
    pipeline_metrics: &TerminalPipelineMetrics,
    observer: &mut Option<MetricsObserver>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::client::{DaemonRequest, MetricsAction};
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let action = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Metrics { action } => Some(action),
            _ => None,
        });
    let Some(action) = action else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let snapshot = (|| {
        let mut broker = metrics
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "metrics are unavailable"))?;
        match action {
            MetricsAction::Subscribe => {
                if observer.is_none() {
                    *observer = Some(broker.subscribe());
                }
                Ok(broker.snapshot())
            }
            MetricsAction::Unsubscribe => {
                if let Some(current) = observer.take() {
                    broker.unsubscribe(current.subscription());
                }
                Ok(broker.snapshot())
            }
            MetricsAction::Snapshot => {
                let (cpu_percent_hundredths, resident_memory_bytes) = process_metrics
                    .lock()
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "process metrics are unavailable",
                        )
                    })?
                    .snapshot();
                let retention = output_pipeline_counters();
                let projection_counters = pr_projection_counters();
                let sampled_at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |duration| {
                        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                    });
                Ok(broker.publish(MetricsSample {
                    sampled_at_ms,
                    cpu_percent_hundredths,
                    resident_memory_bytes,
                    terminal_dropped_bytes: retention.dropped_bytes,
                    terminal_coalesced_bytes: retention.coalesced_bytes,
                    terminal_backpressured_bytes: pipeline_metrics
                        .backpressured_bytes
                        .load(Ordering::Relaxed),
                    pr_projection_dropped_bytes: projection_counters.dropped_bytes,
                    pr_projection_coalesced_bytes: projection_counters.coalesced_bytes,
                    pr_projection_gaps: projection_counters.gaps,
                }))
            }
        }
    })();
    match snapshot {
        Ok(snapshot) => envelope(
            hello,
            request_id,
            ResponseOutcome::Ok,
            serde_json::json!(snapshot),
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

pub(super) struct SessionDispatchContext<'a> {
    pub(super) bound: &'a ConnectionWorkspace,
    pub(super) teardown: &'a TeardownSignal,
    pub(super) agent: &'a SharedAgentRuntime,
    pub(super) pr_inventory: &'a SharedPrInventory,
    pub(super) supervisor: &'a SharedSupervisorRuntime,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_create_reaches_daemon_and_durable_lifecycle
pub(super) fn dispatch_session(
    context: &SessionDispatchContext<'_>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::client::DaemonRequest;
    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Session {
                action,
                operation_id,
                payload,
            } => Some((action, operation_id, payload)),
            _ => None,
        });
    let Some((action, operation_id, payload)) = request else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let result = dispatch_session_action(context, action, &operation_id, &payload);
    session_response_envelope(action, result, request_id, hello)
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_hook_capture_works_without_an_inherited_credential
pub(super) fn request_mcp_credential(body: &serde_json::Value) -> Option<&str> {
    body.get("caller_context")
        .and_then(|context| context.get("credential"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            body.get("payload")
                .and_then(|payload| payload.get("_caller_credential"))
                .and_then(serde_json::Value::as_str)
        })
}

#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_hook_capture_works_without_an_inherited_credential
#[allow(clippy::too_many_arguments)] // Claim binds workspace roots, exact peer identity, and the admitted transport in one response.
pub(super) fn dispatch_mcp_child_claim(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    data_dir: &Path,
    peer_process: &PeerProcess,
    connection: ConnectionId,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::client::DaemonRequest;
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let result = (|| {
        if !matches!(
            serde_json::from_value::<DaemonRequest>(body.clone()),
            Ok(DaemonRequest::McpChildClaim)
        ) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid MCP child claim",
            ));
        }
        let (credential, session_id) = {
            let mut runtime = agent.lock().map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
            })?;
            let (parent_pid, process_group) = peer_process.lineage.ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "MCP child process lineage is unavailable",
                )
            })?;
            let peer_start_identity =
                peer_process
                    .process_start_identity
                    .as_deref()
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::OwnershipUnknown,
                            "MCP child process identity is unavailable",
                        )
                    })?;
            let credential = runtime.claim_mcp_child(
                peer_process.pid,
                peer_start_identity,
                parent_pid,
                process_group,
                connection,
                &|pid, expected_identity| match process_start_identity(pid) {
                    Ok(actual_identity) => actual_identity == expected_identity,
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                },
            )?;
            let session_id = runtime.caller_session(&credential);
            (credential, session_id)
        };
        let store_root = if let Some(session_id) = session_id {
            bound
                .sessions()
                .lock()
                .map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::Unavailable,
                        "MCP caller session scope is unavailable",
                    )
                })?
                .session_scope_by_id(session_id)
                .map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::Unavailable,
                        "MCP caller session scope is unavailable",
                    )
                })?
                .path
        } else {
            bound.tenant.root().to_path_buf()
        };
        let memory_root = data_dir
            .join("agent-memory")
            .join(bound.tenant.workspace_id().to_string())
            .canonicalize()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "agent memory store is unavailable")
            })?;
        validate_owned_directory(&memory_root).map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "agent memory store is unavailable")
        })?;
        Ok((credential, store_root, memory_root))
    })();
    match result {
        Ok((credential, store_root, memory_root)) => envelope(
            hello,
            request_id,
            ResponseOutcome::Ok,
            serde_json::json!({
                "credential": credential,
                "store_root": paths::wire_workspace_root(&store_root),
                "memory_root": paths::wire_workspace_root(&memory_root),
            }),
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_create_reaches_daemon_and_durable_lifecycle
pub(super) fn session_response_envelope(
    action: usagi_core::infrastructure::client::SessionAction,
    result: Result<usagi_daemon::usecase::session_runtime::SessionReply, SessionRuntimeError>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::client::SessionAction;
    use usagi_core::infrastructure::ipc::ResponseOutcome;
    match result {
        Ok(reply) => {
            let outcome = if matches!(action, SessionAction::Create | SessionAction::Remove) {
                ResponseOutcome::Accepted {
                    operation_id: usagi_core::infrastructure::ipc::OperationId(
                        reply.operation_id.clone(),
                    ),
                    operation_revision: reply.revision,
                }
            } else {
                ResponseOutcome::Ok
            };
            // A mutation is synchronously finalized by the lifecycle runtime,
            // but its wire outcome remains Accepted so retries retain the
            // producer-issued operation identity.  Carry the safe final hook
            // beside the snapshot: interactive clients use it to retire their
            // pending UI only after the matching daemon operation completed.
            let mut body = reply.body;
            if let Some(kind) = match action {
                SessionAction::Create => Some("session.created"),
                SessionAction::Remove => Some("session.removed"),
                SessionAction::Clean
                | SessionAction::Sleep
                | SessionAction::List
                | SessionAction::Status
                | SessionAction::Overview
                | SessionAction::Setup
                | SessionAction::Prompt
                | SessionAction::Complete
                | SessionAction::Pr
                | SessionAction::NoteGet
                | SessionAction::NoteUpdate
                | SessionAction::TodoList
                | SessionAction::TodoAdd
                | SessionAction::TodoUpdate
                | SessionAction::TodoRemove
                | SessionAction::DecisionList
                | SessionAction::DecisionLog
                | SessionAction::DelegateIssue
                | SessionAction::DelegateBrief => None,
            } && let Some(object) = body.as_object_mut()
            {
                object.insert(
                    "hook".to_owned(),
                    serde_json::json!({
                        "kind": kind,
                        "operation_id": reply.operation_id,
                        "revision": reply.revision,
                    }),
                );
            }
            envelope(hello, request_id, outcome, body)
        }
        // A delegation answers with its own structured outcome: the caller has to
        // be able to tell a clean rejection from a session that is still there
        // because its worker's fate is unknown, and a code and a sentence cannot
        // carry that.
        Err(SessionRuntimeError::Delegation(failure)) => {
            let mut error =
                usagi_core::infrastructure::ipc::ProtocolError::new(failure.code, &failure.message);
            error.side_effect = if failure.reconcile.left_side_effect() {
                usagi_core::infrastructure::ipc::SideEffect::PartialOrUnknown
            } else {
                usagi_core::infrastructure::ipc::SideEffect::None
            };
            error.details = Some(failure.details());
            envelope(
                hello,
                request_id,
                ResponseOutcome::Error(error),
                serde_json::json!(null),
            )
        }
        Err(error) => {
            let code = match &error {
                SessionRuntimeError::IdempotencyConflict => {
                    usagi_core::infrastructure::ipc::ErrorCode::IdempotencyConflict
                }
                SessionRuntimeError::AgentFailure { code, .. } => *code,
                SessionRuntimeError::Delivery(_) => {
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable
                }
                SessionRuntimeError::PermissionDenied => {
                    usagi_core::infrastructure::ipc::ErrorCode::PermissionDenied
                }
                _ => usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument,
            };
            envelope(
                hello,
                request_id,
                ResponseOutcome::Error(usagi_core::infrastructure::ipc::ProtocolError::new(
                    code,
                    error.safe_message(),
                )),
                serde_json::json!(null),
            )
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down
pub(super) fn exact_merged_pr_head(
    inventory: Option<usagi_core::infrastructure::client::PrSnapshot>,
    branch_head: Option<String>,
) -> Option<String> {
    inventory.and_then(|inventory| {
        branch_head.and_then(|head| {
            inventory.entries.into_iter().find_map(|entry| {
                (entry.state == usagi_core::domain::pr_inventory::PrState::Merged
                    && entry.head_oid.as_deref() == Some(head.as_str()))
                .then_some(head.clone())
            })
        })
    })
}

pub(super) fn best_effort_merged_pr_head(
    inventory: &SharedPrInventory,
    session_id: SessionId,
    branch_head: Option<String>,
) -> Option<String> {
    // PR state is optional evidence for squash-merge branch deletion. If its
    // independent projection is unavailable, retain Git's safe `branch -d`
    // behavior instead of blocking worktree removal.
    let snapshot = inventory
        .lock()
        .ok()
        .and_then(|mut inventory| inventory.snapshot(session_id).ok());
    exact_merged_pr_head(snapshot, branch_head)
}

/// Reconcile the daemon-owned lifecycle set with Git's managed namespace.
///
/// This runs inside the daemon that already owns the workspace fence. Every
/// deletion re-reads lifecycle state immediately before touching Git, so a
/// resource that became linked after inventory cannot be removed.
#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-01-31 tests=running_daemon_cleans_a_merged_orphan_branch_without_touching_active_sessions
#[allow(clippy::too_many_lines)]
pub(super) fn clean_orphan_session_resources(
    bound: &ConnectionWorkspace,
    agent: Option<&SharedAgentRuntime>,
    apply: bool,
    force: bool,
) -> Result<serde_json::Value, SessionRuntimeError> {
    use usagi_core::infrastructure::git::{delete_branch, remove_worktree};
    use usagi_core::usecase::clean::{CleanCandidate, CleanInventory, DaemonWorkspaceData, plan};

    let lifecycle = || -> Result<(PathBuf, BTreeSet<String>), SessionRuntimeError> {
        let sessions = bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?;
        let root = sessions.repository_root().to_path_buf();
        let snapshot = sessions
            .snapshot()
            .map_err(|_| SessionRuntimeError::Storage)?;
        let items = snapshot
            .get("sessions")
            .and_then(serde_json::Value::as_array)
            .ok_or(SessionRuntimeError::Storage)?;
        let names = items
            .iter()
            .map(|item| {
                item.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or(SessionRuntimeError::Storage)
            })
            .collect::<Result<_, _>>()?;
        Ok((root, names))
    };
    let (root, names) = lifecycle()?;
    let repositories = usagi_core::infrastructure::git::observe_repository(&SystemGit, &root)
        .map_err(|error| {
            SessionRuntimeError::DurableFailure(format!(
                "could not inspect orphan session resources: {error}"
            ))
        })?
        .into_iter()
        .collect();
    let candidates = plan(&CleanInventory {
        daemon_data: vec![DaemonWorkspaceData {
            root: root.clone(),
            dir: PathBuf::new(),
            root_exists: true,
            sessions: Some(names),
        }],
        repositories,
        ..CleanInventory::default()
    });
    let git_candidates = candidates
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate,
                CleanCandidate::Worktree { .. } | CleanCandidate::Branch { .. }
            )
        })
        .collect::<Vec<_>>();
    let failed_reservations = agent.map_or_else(
        || Ok(Vec::new()),
        |agent| {
            agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .failed_reservation_ids()
                .map_err(|_| SessionRuntimeError::Storage)
        },
    )?;
    let mut described = git_candidates
        .iter()
        .map(|candidate| match candidate {
            CleanCandidate::Worktree {
                path,
                requires_force,
                ..
            } => serde_json::json!({
                "kind": "worktree",
                "name": path.file_name().and_then(|name| name.to_str()),
                "path": path,
                "protected": requires_force,
            }),
            CleanCandidate::Branch {
                name,
                requires_force,
                ..
            } => serde_json::json!({
                "kind": "branch",
                "name": name,
                "protected": requires_force,
            }),
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    described.extend(failed_reservations.iter().map(|runtime| {
        serde_json::json!({
            "kind": "agent_reservation",
            "name": runtime.as_str(),
            "protected": true,
        })
    }));
    if !apply {
        return Ok(serde_json::json!({
            "mode": "dry_run",
            "candidates": described,
            "removed": 0,
            "protected": git_candidates.iter().filter(|item| item.requires_force()).count()
                + failed_reservations.len(),
        }));
    }

    let mut removed = 0usize;
    let mut protected = 0usize;
    if !failed_reservations.is_empty() {
        if force {
            removed += agent
                .ok_or(SessionRuntimeError::InvalidRequest)?
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .clean_failed_reservations()
                .map_err(|error| {
                    SessionRuntimeError::DurableFailure(format!(
                        "could not clean failed Agent reservations: {}",
                        error.message
                    ))
                })?;
        } else {
            protected += failed_reservations.len();
        }
    }
    for candidate in &git_candidates {
        if candidate.requires_force() && !force {
            protected += 1;
            continue;
        }
        let name = match candidate {
            CleanCandidate::Worktree { path, .. } => path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(SessionRuntimeError::InvalidRequest)?,
            CleanCandidate::Branch { name, .. } => name
                .strip_prefix("usagi/")
                .ok_or(SessionRuntimeError::InvalidRequest)?,
            _ => unreachable!(),
        };
        let (_, current) = lifecycle()?;
        if current.contains(name) {
            return Err(SessionRuntimeError::DurableFailure(format!(
                "orphan cleanup stopped because session \"{name}\" became active"
            )));
        }
        let result = match candidate {
            CleanCandidate::Worktree { path, .. } => {
                remove_worktree(&SystemGit, &root, path, candidate.requires_force() && force)
            }
            CleanCandidate::Branch { name, .. } => {
                delete_branch(&SystemGit, &root, name, candidate.requires_force() && force)
            }
            _ => unreachable!(),
        };
        result.map_err(|error| {
            SessionRuntimeError::DurableFailure(format!(
                "could not clean orphan session resource \"{name}\": {error}"
            ))
        })?;
        removed += 1;
    }
    Ok(serde_json::json!({
        "mode": "apply",
        "candidates": described,
        "removed": removed,
        "protected": protected,
    }))
}

#[allow(clippy::too_many_lines)]
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_brief_immediately_dispatches_an_isolated_triage_worker
pub(super) fn dispatch_session_action(
    context: &SessionDispatchContext<'_>,
    action: usagi_core::infrastructure::client::SessionAction,
    operation_id: &str,
    payload: &serde_json::Value,
) -> Result<usagi_daemon::usecase::session_runtime::SessionReply, SessionRuntimeError> {
    use usagi_core::infrastructure::client::SessionAction;
    use usagi_core::infrastructure::store::{issue::IssueStore, state::WorkspaceStateStore};
    use usagi_core::usecase::{issue, note};
    use usagi_daemon::usecase::agent_ipc::PromptMode;

    let bound = context.bound;
    let teardown = context.teardown;
    let agent = context.agent;
    let pr_inventory = context.pr_inventory;

    let authenticated_caller = payload
        .get("_caller_credential")
        .and_then(serde_json::Value::as_str)
        .map(|credential| {
            agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .mcp_dispatch_context(credential)
                .ok_or(SessionRuntimeError::ScopeUnavailable)
        })
        .transpose()?;

    let reply = |body: serde_json::Value| {
        let revision = bound
            .sessions()
            .lock()
            .ok()
            .and_then(|runtime| runtime.snapshot().ok())
            .and_then(|snapshot| snapshot.get("revision").and_then(serde_json::Value::as_u64))
            .unwrap_or_default();
        Ok(usagi_daemon::usecase::session_runtime::SessionReply {
            operation_id: operation_id.to_owned(),
            revision,
            body,
        })
    };
    let string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(SessionRuntimeError::InvalidRequest)
    };
    let caller_scope = || {
        let credential = string("_caller_credential")?;
        let session_id = agent
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .caller_session(credential)
            .ok_or(SessionRuntimeError::ScopeUnavailable)?;
        bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .session_scope_by_id(session_id)
    };
    let bound_workspace = || {
        bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .workspace_id()
            .map_err(|_| SessionRuntimeError::Storage)
    };
    if let Some(authenticated) = authenticated_caller.as_ref()
        && authenticated.workspace_id != bound_workspace()?
    {
        return Err(SessionRuntimeError::ScopeUnavailable);
    }
    let caller = authenticated_caller.as_ref().map(|caller| &caller.caller);
    let target_session = |name: &str| {
        let sessions = bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?;
        if let Some(caller) = caller {
            sessions.created_session_id(name, caller)
        } else {
            sessions.session_id(name)
        }
    };
    let authorize_create_or_reuse = |name: &str| {
        if let Some(caller) = caller {
            bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .authorize_create_or_reuse(name, caller)?;
        }
        Ok::<(), SessionRuntimeError>(())
    };

    match action {
        SessionAction::List | SessionAction::Status | SessionAction::Overview => {
            let visible = caller
                .map(|caller| {
                    bound
                        .sessions()
                        .lock()
                        .map_err(|_| SessionRuntimeError::Storage)?
                        .created_session_ids(caller)
                })
                .transpose()?;
            let mut status = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(action, operation_id, payload)?;
            let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
            let store = runtime.dispatch_store();
            let agents = store.agents().map_err(|_| SessionRuntimeError::Storage)?;
            if let Some(items) = status
                .body
                .get_mut("sessions")
                .and_then(serde_json::Value::as_array_mut)
            {
                for item in items.iter_mut() {
                    if let Some(id) = item
                        .get("session_id")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok())
                    {
                        item["agent_phase"] = serde_json::json!(runtime.session_phase(id));
                        let (resumable, reason) = runtime.session_resume_status(id);
                        item["agent_resumable"] = serde_json::json!(resumable);
                        item["agent_resume_reason"] = serde_json::json!(reason);
                        item["agent_status"] = serde_json::json!(aggregate_agent_status(
                            agents
                                .iter()
                                .filter(|agent| agent.session_id == Some(id))
                                .map(|agent| agent.status),
                        ));
                        // Parentage is immutable lifecycle metadata captured when
                        // the session is created. A later dispatch into an existing
                        // session must never reorganize it.
                        if item.get("parent_session_id").is_none() {
                            item["parent_session_id"] = serde_json::Value::Null;
                        }
                    }
                }
                let names = items
                    .iter()
                    .filter_map(|item| {
                        Some((
                            serde_json::from_value(item.get("session_id")?.clone()).ok()?,
                            item.get("name")?.as_str()?.to_owned(),
                        ))
                    })
                    .collect::<BTreeMap<SessionId, String>>();
                let parents = items
                    .iter()
                    .filter_map(|item| {
                        let id = serde_json::from_value(item.get("session_id")?.clone()).ok()?;
                        let parent = item
                            .get("parent_session_id")
                            .filter(|value| !value.is_null())
                            .cloned()
                            .and_then(|value| serde_json::from_value(value).ok());
                        Some((id, parent))
                    })
                    .collect::<BTreeMap<SessionId, Option<SessionId>>>();
                for item in items.iter_mut() {
                    let Some(id) = item
                        .get("session_id")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok())
                    else {
                        continue;
                    };
                    let parent = parents.get(&id).copied().flatten();
                    item["parent_session_name"] =
                        serde_json::json!(parent.and_then(|id| names.get(&id)));
                    let mut lineage = Vec::new();
                    let mut cursor = Some(id);
                    let mut seen = BTreeSet::new();
                    while let Some(member) = cursor
                        && seen.insert(member)
                    {
                        lineage.push(member);
                        cursor = parents
                            .get(&member)
                            .copied()
                            .flatten()
                            .filter(|parent| names.contains_key(parent));
                    }
                    lineage.reverse();
                    item["organization_depth"] = serde_json::json!(lineage.len());
                    let mut path = vec!["Director".to_owned()];
                    path.extend(
                        lineage
                            .iter()
                            .filter_map(|member| names.get(member).cloned()),
                    );
                    item["organization_path"] = serde_json::json!(path);
                }
                if let Some(visible) = visible.as_ref() {
                    items.retain(|item| {
                        item.get("session_id")
                            .cloned()
                            .and_then(|value| serde_json::from_value(value).ok())
                            .is_some_and(|id| visible.contains(&id))
                    });
                }
            }
            Ok(status)
        }
        SessionAction::Prompt => {
            let name = string("name")?;
            let prompt = string("prompt")?;
            let target = if name == ":root" {
                if caller.is_some() {
                    return Err(SessionRuntimeError::PermissionDenied);
                }
                None
            } else {
                Some(target_session(name)?)
            };
            let mode = match payload
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("live")
            {
                "queue" => PromptMode::Queue,
                "live" => PromptMode::Live,
                _ => return Err(SessionRuntimeError::InvalidRequest),
            };
            let workspace = bound_workspace()?;
            let delivery = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .prompt(workspace, target, prompt, mode)
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            reply(
                serde_json::json!({"name": name, "delivered_to": delivery.delivered_to, "queued": delivery.queued}),
            )
        }
        SessionAction::Complete => {
            let message = string("message")?;
            let credential = string("_caller_credential")?;
            let scope = caller_scope()?;
            let delivery = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .report_from_mcp(
                    credential,
                    None,
                    usagi_core::domain::agent::InboxKind::Completed,
                    message.to_owned(),
                    None,
                )
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            reply(serde_json::json!({
                "session_id": scope.session_id,
                "reported_to": delivery.delivered_to,
                "delivered_to": "inbox"
            }))
        }
        SessionAction::Pr => {
            let (name, id) = if payload.get("name").is_some() {
                let name = string("name")?;
                (name.to_owned(), target_session(name)?)
            } else {
                let id = caller_scope()?.session_id;
                let lifecycle = bound
                    .sessions()
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .snapshot()
                    .map_err(|_| SessionRuntimeError::Storage)?;
                let name = lifecycle
                    .get("sessions")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|sessions| {
                        sessions.iter().find(|session| {
                            session.get("session_id") == Some(&serde_json::json!(id))
                                && session.get("lifecycle").and_then(serde_json::Value::as_str)
                                    == Some("available")
                        })
                    })
                    .and_then(|session| session.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .ok_or(SessionRuntimeError::ScopeUnavailable)?
                    .to_owned();
                (name, id)
            };
            let snapshot = pr_inventory
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .snapshot(id)
                .map_err(|_| SessionRuntimeError::Storage)?;
            let merged = snapshot
                .entries
                .iter()
                .any(|entry| entry.state == usagi_core::domain::pr_inventory::PrState::Merged);
            reply(
                serde_json::json!({"name": name, "session_id": id, "revision": snapshot.revision, "merged": merged, "pr": snapshot.entries}),
            )
        }
        SessionAction::NoteGet
        | SessionAction::NoteUpdate
        | SessionAction::TodoList
        | SessionAction::TodoAdd
        | SessionAction::TodoUpdate
        | SessionAction::TodoRemove
        | SessionAction::DecisionList
        | SessionAction::DecisionLog => {
            let scope = caller_scope()?;
            let store = WorkspaceStateStore::new(&scope.path);
            let target = note::Target::Root;
            let body = match action {
                SessionAction::NoteGet => {
                    serde_json::json!({"note": note::note(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::NoteUpdate => {
                    let value = payload
                        .get("note")
                        .and_then(serde_json::Value::as_str)
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    note::set_note(&store, target, value, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    serde_json::json!({"note": note::note(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoList => {
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoAdd => {
                    let text = string("text")?;
                    note::add_todo(&store, target, text, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoUpdate => {
                    let index = payload
                        .get("index")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    let done = payload
                        .get("done")
                        .map(|value| value.as_bool().ok_or(SessionRuntimeError::InvalidRequest))
                        .transpose()?;
                    let text = payload
                        .get("text")
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_owned)
                                .ok_or(SessionRuntimeError::InvalidRequest)
                        })
                        .transpose()?;
                    if done.is_none() && text.is_none() {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    if !note::update_todo(&store, target, index, done, text, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?
                    {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoRemove => {
                    let index = payload
                        .get("index")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    if !note::remove_todo(&store, target, index, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?
                    {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::DecisionList => {
                    serde_json::json!({"decisions": note::decisions(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::DecisionLog => {
                    let text = string("text")?;
                    note::log_decision(&store, target, text, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    serde_json::json!({"decisions": note::decisions(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                _ => unreachable!(),
            };
            reply(serde_json::json!({"session_id": scope.session_id, "scratchpad": body}))
        }
        SessionAction::DelegateBrief => reply(delegate_brief(context, operation_id, payload)?),
        SessionAction::DelegateIssue => {
            let workspace = bound_workspace()?;
            let number = payload
                .get("number")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or(SessionRuntimeError::InvalidRequest)?;
            let name = payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| format!("issue-{number}"), str::to_owned);
            let _delegation_permit =
                authorize_delegation(bound, agent, caller, payload.get("role"), operation_id)?;
            authorize_create_or_reuse(&name)?;
            let root = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .repository_root()
                .to_path_buf();
            let issue = issue::get(&IssueStore::new(root), number)
                .map_err(|error| {
                    error
                        .chain()
                        .find_map(|cause| cause.downcast_ref::<AmbiguousIssueNumber>())
                        .cloned()
                        .map_or(
                            SessionRuntimeError::Storage,
                            SessionRuntimeError::AmbiguousIssue,
                        )
                })?
                .ok_or(SessionRuntimeError::InvalidRequest)?;
            let prompt = issue::to_prompt(&issue);
            let requested_role = payload.get("role").cloned();
            let mut created = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(
                    SessionAction::Create,
                    operation_id,
                    &serde_json::json!({
                        "name": name,
                        "role": requested_role,
                        "parent_session_id": caller.and_then(|caller| caller.session_id),
                        "creator_agent_id": caller.map(|caller| caller.agent_id),
                    }),
                )?;
            let id = record_session_lineage(agent, workspace, &created.body, &name)?;
            if caller.is_some()
                && let Some(sessions) = created
                    .body
                    .get_mut("sessions")
                    .and_then(serde_json::Value::as_array_mut)
            {
                sessions
                    .retain(|session| session.get("session_id") == Some(&serde_json::json!(id)));
            }
            let delivery = if let Some(caller) = caller.cloned() {
                let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
                runtime
                    .dispatch_store()
                    .queue_delegated_prompt(
                        workspace,
                        Some(id),
                        prompt,
                        chrono::Utc::now(),
                        caller,
                        usagi_core::domain::id::OperationId::parse(operation_id)
                            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
                    )
                    .map_err(|_| SessionRuntimeError::Storage)?;
                usagi_daemon::usecase::agent_ipc::PromptDelivery {
                    delivered_to: "queue",
                    queued: true,
                }
            } else {
                agent
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .prompt(workspace, Some(id), &prompt, PromptMode::Queue)
                    .map_err(|error| SessionRuntimeError::Delivery(error.message))?
            };
            reply(
                serde_json::json!({"name": name, "session_id": id, "created": created.body, "delivered_to": delivery.delivered_to, "queued": delivery.queued}),
            )
        }
        // Create runs its heavy Git worktree build with the shared session lock
        // released, so a long `git worktree add` never freezes concurrent
        // readers (session list, terminal poll, user-decision list) on the
        // daemon. The fast durable transitions still run under the lock.
        SessionAction::Create => {
            authorize_create_or_reuse(string("name")?)?;
            let mut create_payload = payload.clone();
            create_payload["parent_session_id"] =
                serde_json::json!(caller.and_then(|caller| caller.session_id));
            create_payload["creator_agent_id"] =
                serde_json::json!(caller.map(|caller| caller.agent_id));
            let mut created =
                perform_create(bound.sessions(), &SystemGit, operation_id, &create_payload)?;
            let id =
                record_session_lineage(agent, bound_workspace()?, &created.body, string("name")?)?;
            if caller.is_some()
                && let Some(sessions) = created
                    .body
                    .get_mut("sessions")
                    .and_then(serde_json::Value::as_array_mut)
            {
                sessions
                    .retain(|session| session.get("session_id") == Some(&serde_json::json!(id)));
            }
            Ok(created)
        }
        // Remove goes further: it answers as soon as the session is durably
        // `Deleting` and hands the unbounded worktree teardown to the daemon's
        // teardown worker. Keeping the teardown on this connection would hold
        // the reply past every client attempt deadline for a session with a
        // multi-gigabyte `target/`.
        SessionAction::Remove => {
            let name = string("name")?;
            let (id, branch_head) = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .removal_identity(name)?;
            if let Some(caller) = caller {
                let target = bound
                    .sessions()
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .created_session_record_id(name, caller)?;
                if id != target {
                    return Err(SessionRuntimeError::PermissionDenied);
                }
            }
            let merged_head_oid = best_effort_merged_pr_head(pr_inventory, id, branch_head);
            let mut remove_payload = payload.clone();
            remove_payload["parent_session_id"] =
                serde_json::json!(caller.and_then(|caller| caller.session_id));
            remove_payload["creator_agent_id"] =
                serde_json::json!(caller.map(|caller| caller.agent_id));
            perform_remove_with_merged_head(
                bound.sessions(),
                teardown,
                operation_id,
                &remove_payload,
                merged_head_oid,
            )
        }
        SessionAction::Sleep => {
            let name = string("name")?;
            let id = target_session(name)?;
            let slept = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .sleep_session(id)
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            let mut snapshot = dispatch_session_action(
                context,
                SessionAction::List,
                operation_id,
                &serde_json::json!({}),
            )?;
            snapshot.body["slept"] = serde_json::json!(slept);
            snapshot.body["slept_session"] = serde_json::json!(name);
            snapshot.body["session_retained"] = serde_json::json!(true);
            Ok(snapshot)
        }
        SessionAction::Clean => {
            let flag = |name| match payload.get(name) {
                None => Ok(false),
                Some(serde_json::Value::Bool(value)) => Ok(*value),
                Some(_) => Err(SessionRuntimeError::InvalidRequest),
            };
            let apply = flag("apply")?;
            let force = flag("force")?;
            if force && !apply {
                return Err(SessionRuntimeError::InvalidRequest);
            }
            reply(clean_orphan_session_resources(
                bound,
                Some(agent),
                apply,
                force,
            )?)
        }
        SessionAction::Setup => {
            target_session(string("name")?)?;
            bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(action, operation_id, payload)
        }
    }
}

/// Reads a delegation's `agent` selector, which names a runtime and model and
/// nothing else.
///
/// An `agent.id` is refused rather than resolved. No existing Agent can belong to
/// a session the same request is about to create, so the dispatch ownership check
/// would reject every such selector — after the worktree already existed. The
/// tool schema no longer advertises that branch and this is the daemon-side half
/// of the same rule.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_brief_publishes_and_accepts_only_a_new_agent_selector
pub(super) fn new_agent_selector(
    selector: Option<&serde_json::Value>,
) -> Result<
    (
        usagi_core::domain::agent::AgentProfileId,
        usagi_core::domain::agent::ModelSelector,
    ),
    SessionRuntimeError,
> {
    use usagi_core::domain::agent::{AgentProfileId, ModelSelector};

    let selector = selector
        .and_then(serde_json::Value::as_object)
        .filter(|selector| selector.len() == 2 && !selector.contains_key("id"))
        .ok_or(SessionRuntimeError::InvalidRequest)?;
    let field = |key: &str| {
        selector
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    Ok((
        serde_json::from_value::<AgentProfileId>(field("runtime"))
            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
        serde_json::from_value::<ModelSelector>(field("model"))
            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
    ))
}

/// Applies configured company-role authority before any delegated side effect.
/// Catalogs without a `delegation` block keep their established permissive
/// behavior; once a block is present the daemon owns every decision.
pub(super) struct DelegationPermit {
    store: Option<usagi_core::infrastructure::store::dispatch::DispatchStore>,
    operation_id: Option<usagi_core::domain::id::OperationId>,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_issue_counts_queued_children_against_concurrency
impl DelegationPermit {
    const fn inert() -> Self {
        Self {
            store: None,
            operation_id: None,
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_issue_counts_queued_children_against_concurrency
impl Drop for DelegationPermit {
    fn drop(&mut self) {
        if let (Some(store), Some(operation_id)) = (&self.store, self.operation_id) {
            let _ = store.release_delegation(operation_id);
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_issue_counts_queued_children_against_concurrency
pub(super) fn authorize_delegation(
    bound: &ConnectionWorkspace,
    agent: &SharedAgentRuntime,
    caller: Option<&usagi_core::domain::agent::CallerRef>,
    requested_role: Option<&serde_json::Value>,
    operation_id: &str,
) -> Result<DelegationPermit, SessionRuntimeError> {
    use usagi_core::domain::role::{RoleId, RoleScope};
    use usagi_core::infrastructure::store::dispatch::DelegationReservationOutcome;

    let requested = requested_role
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value::<RoleId>)
        .transpose()
        .map_err(|_| SessionRuntimeError::InvalidRequest)?;
    let sessions = bound
        .sessions()
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?;
    let catalog = sessions.effective_role_catalog()?;
    let Some(caller) = caller else {
        if catalog
            .roles
            .values()
            .any(|definition| definition.delegation.is_some())
        {
            return Err(SessionRuntimeError::InvalidRole(
                "authenticated caller is required by the delegation policy".into(),
            ));
        }
        return Ok(DelegationPermit::inert());
    };
    let parent_role = match caller.session_id {
        Some(id) => sessions.session_role(id)?,
        None => catalog
            .resolve(None, RoleScope::Root)
            .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))?,
    };
    let child_role = catalog
        .resolve(requested.as_ref(), RoleScope::Session)
        .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))?;
    let Some(policy) = parent_role
        .as_ref()
        .and_then(|role| catalog.roles.get(role))
        .and_then(|definition| definition.delegation.as_ref())
    else {
        return Ok(DelegationPermit::inert());
    };
    if !policy.enabled {
        return Err(SessionRuntimeError::InvalidRole(
            "caller role is not allowed to delegate".into(),
        ));
    }
    let child_role = child_role.ok_or_else(|| {
        SessionRuntimeError::InvalidRole("delegation requires an authorized child role".into())
    })?;
    if !policy.child_roles.contains(&child_role) {
        return Err(SessionRuntimeError::InvalidRole(format!(
            "caller role may not delegate to role \"{child_role}\""
        )));
    }
    drop(sessions);

    let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
    let store = runtime.dispatch_store();
    let depth = store
        .delegation_depth(caller)
        .map_err(|_| SessionRuntimeError::Storage)?;
    if depth.saturating_add(1) > policy.max_depth {
        return Err(SessionRuntimeError::InvalidRole(format!(
            "delegation depth limit ({}) reached",
            policy.max_depth
        )));
    }
    let operation_id = usagi_core::domain::id::OperationId::parse(operation_id)
        .map_err(|_| SessionRuntimeError::InvalidRequest)?;
    match store
        .reserve_delegation(caller, operation_id, policy.max_concurrency)
        .map_err(|_| SessionRuntimeError::Storage)?
    {
        DelegationReservationOutcome::Reserved => Ok(DelegationPermit {
            store: Some(store.clone()),
            operation_id: Some(operation_id),
        }),
        DelegationReservationOutcome::AlreadyAdmitted => Ok(DelegationPermit::inert()),
        DelegationReservationOutcome::LimitReached => {
            Err(SessionRuntimeError::InvalidRole(format!(
                "delegation concurrency limit ({}) reached",
                policy.max_concurrency
            )))
        }
        DelegationReservationOutcome::InProgress => Err(SessionRuntimeError::InvalidRole(
            "delegation operation is already in progress".into(),
        )),
    }
}

pub(super) fn required_payload_string<'a>(
    payload: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, SessionRuntimeError> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(SessionRuntimeError::InvalidRequest)
}

/// Creates a triage session for a brief and dispatches a fresh worker into it,
/// as one operation that either takes effect completely or leaves nothing.
///
/// The order is what makes that true. Every rejection the daemon can decide
/// without a side effect — the selector, the caller, the runtime/model
/// allowlist, the runtime executable, an operation that already owns an
/// admission — is decided before the worktree exists. Only after that does the
/// create run, and a dispatch that then fails definitively is rolled back by the
/// same durable teardown `session_remove` uses, which the daemon resumes across
/// a restart. A dispatch whose spawn outcome is *unknown* is deliberately not
/// rolled back: the worktree may already hold a running worker, so the caller
/// gets the session and run identity to reconcile instead.
#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_delegate_brief_immediately_dispatches_an_isolated_triage_worker
#[allow(clippy::too_many_lines)] // Atomic create, reservation, spawn, compensation, and recovery stay visible as one transaction.
pub(super) fn delegate_brief(
    context: &SessionDispatchContext<'_>,
    operation_id: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, SessionRuntimeError> {
    use usagi_core::infrastructure::client::{DispatchAgentIntent, DispatchIntent};

    let bound = context.bound;
    let teardown = context.teardown;
    let agent = context.agent;
    let supervisor = context.supervisor;

    let brief = required_payload_string(payload, "brief")?;
    let suffix = operation_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>();
    let name = payload
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| format!("triage-{suffix}"), str::to_owned);
    let prompt = format!(
        "このセッションの worktree 内で次の依頼をトリアージし、必要なら issue 化して実装へつなげてください。リポジトリの規約に従ってください。\n\n{brief}"
    );
    let (runtime, model) = new_agent_selector(payload.get("agent"))?;

    let credential = required_payload_string(payload, "_caller_credential")?;
    let (workspace, parent_dispatch_run, caller, repository_root) = {
        let agent_runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
        let authenticated = agent_runtime
            .mcp_dispatch_context(credential)
            .ok_or(SessionRuntimeError::ScopeUnavailable)?;
        let sessions = bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?;
        let workspace = sessions
            .snapshot()
            .map_err(|_| SessionRuntimeError::Storage)?
            .get("workspace_id")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or(SessionRuntimeError::Storage)?;
        if authenticated.workspace_id != workspace {
            return Err(SessionRuntimeError::ScopeUnavailable);
        }
        sessions.authorize_create_or_reuse(&name, &authenticated.caller)?;
        (
            workspace,
            authenticated.run_id,
            authenticated.caller,
            sessions.repository_root().to_path_buf(),
        )
    };
    let supervision_at_preflight = supervisor
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .supervision_fence(parent_dispatch_run)
        .map_err(|_| SessionRuntimeError::Storage)?;
    if supervision_at_preflight.is_some() {
        agent
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .require_same_dispatch_runtime(
                workspace,
                &caller,
                &DispatchAgentIntent::New {
                    runtime: runtime.clone(),
                    model: model.clone(),
                },
            )
            .map_err(|error| SessionRuntimeError::AgentFailure {
                code: error.code,
                message: error.message,
            })?;
    }
    let _delegation_permit = authorize_delegation(
        bound,
        agent,
        Some(&caller),
        payload.get("role"),
        operation_id,
    )?;
    // Machine-local runtime/model policy belongs to the workspace root and is
    // not copied into managed worktrees. Decide every read-only refusal here;
    // `dispatch` still re-reads the same trusted root and stays the authority.
    agent
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .preflight_dispatch(operation_id, &prompt, &runtime, &model, &repository_root)
        .map_err(|error| SessionRuntimeError::AgentFailure {
            code: error.code,
            message: error.message,
        })?;

    let mut created = perform_delegated_create(
        bound.sessions(),
        &SystemGit,
        operation_id,
        &serde_json::json!({
            "name": name,
            "role": payload.get("role").cloned(),
            "parent_session_id": caller.session_id,
            "creator_agent_id": caller.agent_id,
        }),
    )?;
    let id = session_id_by_name(&created.body, &name).ok_or(SessionRuntimeError::Storage)?;
    if record_session_lineage(agent, workspace, &created.body, &name).is_err() {
        return Err(compensate_delegation(
            bound.sessions(),
            teardown,
            id,
            &name,
            operation_id,
            usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                "session parentage is unavailable",
            ),
        ));
    }
    if let Some(sessions) = created
        .body
        .get_mut("sessions")
        .and_then(serde_json::Value::as_array_mut)
    {
        sessions.retain(|session| session.get("session_id") == Some(&serde_json::json!(id)));
    }
    let selected = DispatchAgentIntent::New {
        runtime: runtime.clone(),
        model: model.clone(),
    };
    let reserved_worker = if supervision_at_preflight.is_some() {
        let planned = agent
            .lock()
            .map_err(|_| {
                usagi_core::infrastructure::ipc::ProtocolError::new(
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                    "agent owner is unavailable",
                )
            })
            .and_then(|runtime| runtime.plan_dispatch_worker(workspace, id, &selected));
        match planned {
            Ok(worker) => Some(worker),
            Err(error) => {
                return Err(compensate_delegation(
                    bound.sessions(),
                    teardown,
                    id,
                    &name,
                    operation_id,
                    error,
                ));
            }
        }
    } else {
        None
    };
    let scope = bound.scope_resolver();
    let reservation = (|| {
        let runtime = supervisor.lock().map_err(|_| {
            usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                "supervisor runtime is unavailable",
            )
        })?;
        let supervision_before_reservation = runtime
            .supervision_fence(parent_dispatch_run)
            .map_err(supervisor_error)?;
        require_stable_supervisor_fence(
            supervision_at_preflight.as_ref(),
            supervision_before_reservation.as_ref(),
        )?;
        let reservation = if let Some(reserved_worker) = reserved_worker.as_ref() {
            runtime
                .reserve_delegated_dispatch_for_session(
                    parent_dispatch_run,
                    operation_id,
                    prompt.clone(),
                    id,
                    reserved_worker,
                    &name,
                    chrono::Utc::now(),
                )
                .map_err(supervisor_error)?
        } else {
            None
        };
        let supervision_after_reservation = runtime
            .supervision_fence(parent_dispatch_run)
            .map_err(supervisor_error)?;
        require_stable_supervisor_fence(
            supervision_at_preflight.as_ref(),
            supervision_after_reservation.as_ref(),
        )?;
        require_supervisor_reservation_presence(
            supervision_at_preflight.as_ref(),
            reservation.is_some(),
        )?;
        Ok(reservation)
    })()
    .map_err(|error| {
        compensate_delegation(bound.sessions(), teardown, id, &name, operation_id, error)
    })?;
    let supervised = reservation.is_some();
    let prompt = reservation.map_or(prompt, |reservation| reservation.prompt);
    let dispatch_intent = DispatchIntent {
        workspace,
        session_name: name.clone(),
        caller,
        agent: selected,
        prompt,
    };
    let admission = dispatch_agent_after_preflight(
        agent,
        operation_id,
        &dispatch_intent,
        id,
        &scope,
        reserved_worker.as_ref(),
    );
    let admission = match admission {
        Ok(admission) => admission,
        Err(error) => {
            if supervised
                && error.code != usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown
                && let Ok(runtime) = supervisor.lock()
                && let Err(failure) =
                    runtime.fail_reserved_delegated_dispatch(operation_id, chrono::Utc::now())
            {
                ErrorLog::record(&format!(
                    "delegated Supervisor failure reconciliation deferred: {failure}"
                ));
            }
            return Err(compensate_delegation(
                bound.sessions(),
                teardown,
                id,
                &name,
                operation_id,
                error,
            ));
        }
    };
    if supervised
        && let Err(error) = bind_delegated_supervisor_dispatch(
            supervisor,
            &admission.operation_id,
            &admission.runtime,
        )
    {
        // The child Agent is already durable; exact-operation reconciliation
        // finishes the promotion without asking the caller to retry the spawn.
        ErrorLog::record(&format!("delegated Supervisor promotion deferred: {error}"));
        if let Err(reconcile) = reconcile_pending_supervisor_promotions(supervisor, agent) {
            ErrorLog::record(&format!(
                "delegated Supervisor promotion reconciliation deferred: {reconcile}"
            ));
        }
    }
    Ok(serde_json::json!({
        "name": name,
        "session_id": id,
        "created": created.body,
        "run_id": admission.operation_id,
        "terminal": admission.terminal,
        "completed": admission.completed,
    }))
}

/// Rolls a delegated create back, or reports why it must not be rolled back.
///
/// The teardown is admitted under a fresh operation identity because the
/// delegation's own identity already names the create it is compensating. Once
/// admitted it is durable: the daemon's teardown worker finishes it, and a
/// daemon that dies first resumes it from the `Deleting` record on the next
/// start.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_failed_delegation_reports_its_reconcile_state_on_the_wire
pub(super) fn compensate_delegation(
    sessions: &SharedSessionRuntime,
    teardown: &TeardownSignal,
    session_id: usagi_core::domain::id::SessionId,
    name: &str,
    run_operation_id: &str,
    error: usagi_core::infrastructure::ipc::ProtocolError,
) -> SessionRuntimeError {
    use usagi_daemon::usecase::session_runtime::{DelegationFailure, DelegationReconcile};

    let reconcile = if error.code == usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown {
        DelegationReconcile::Retained
    } else {
        match perform_compensating_remove(
            sessions,
            teardown,
            &usagi_core::domain::id::OperationId::new().to_string(),
            name,
        ) {
            // A session that is already gone needs no compensation: an earlier
            // attempt's teardown removed it, so nothing was left behind either
            // way.
            Ok(_) | Err(SessionRuntimeError::UnknownSession) => DelegationReconcile::Compensated,
            Err(_) => DelegationReconcile::CompensationFailed,
        }
    };
    SessionRuntimeError::Delegation(DelegationFailure {
        code: error.code,
        message: error.message,
        session_id,
        run_operation_id: run_operation_id.to_owned(),
        reconcile,
    })
}

/// Compensates delegated creates whose dispatch never became durable.
///
/// A delegation builds its worktree before it can dispatch into it, so a daemon
/// that died inside that window left an available session no caller owns and no
/// run points at. This runs before the daemon accepts connections, so no client
/// ever observes such a session, and it uses the same durable teardown a live
/// compensation does.
///
/// A reservation in the dispatch store — even one a restart already failed — is
/// not an orphan: that operation reached the dispatch side, which owns its
/// outcome. Only a create with nothing at all behind it is rolled back.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_failed_delegation_reports_its_reconcile_state_on_the_wire
pub(super) fn reconcile_orphan_delegations(
    bound: &ConnectionWorkspace,
    dispatch: &DispatchStore,
    teardown: &TeardownSignal,
) -> usize {
    let Ok(candidates) = bound
        .sessions()
        .lock()
        .map_err(|_| ())
        .and_then(|sessions| sessions.delegated_sessions().map_err(|_| ()))
    else {
        return 0;
    };
    candidates
        .into_iter()
        .filter(|candidate| {
            matches!(dispatch.run(candidate.operation_id), Ok(None))
                && matches!(dispatch.admission(candidate.operation_id), Ok(None))
        })
        .filter(|candidate| {
            perform_compensating_remove(
                bound.sessions(),
                teardown,
                &usagi_core::domain::id::OperationId::new().to_string(),
                &candidate.name,
            )
            .is_ok()
        })
        .count()
}

pub(super) enum AgentDispatchRequest {
    Launch(
        String,
        usagi_core::infrastructure::client::AgentLaunchIntent,
    ),
    Goal(String, usagi_core::infrastructure::client::AgentGoalIntent),
    Inventory(WorkspaceId),
    WorkspaceObservation(WorkspaceId),
    Diagnose(
        WorkspaceId,
        Vec<usagi_core::domain::agent::AgentIntegrationRevision>,
    ),
    PlanRestart(
        Vec<usagi_core::domain::agent::AgentIntegrationRevision>,
        bool,
    ),
    Restart(
        WorkspaceId,
        Vec<usagi_core::domain::agent::AgentIntegrationRevision>,
        Vec<usagi_core::domain::id::AgentRuntimeRef>,
        bool,
    ),
    Resume(String, usagi_core::domain::agent::AgentResumeTarget),
    RepairResume(String, usagi_core::domain::agent::AgentResumeTarget, u32),
}

#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
#[allow(clippy::too_many_lines)] // Preflight, reservation, and Supervisor escalation are one fail-closed admission boundary.
pub(super) fn admit_agent_dispatch_request(
    agent: &SharedAgentRuntime,
    supervisor: &SharedSupervisorRuntime,
    scope: &dyn SessionScopeResolver,
    request: &AgentDispatchRequest,
) -> Result<AgentDispatchAdmission, usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let preflight = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|owner| match request {
            AgentDispatchRequest::Launch(operation_id, intent) => {
                owner.prepare_launch_readiness(operation_id, intent)
            }
            AgentDispatchRequest::Goal(operation_id, intent) => {
                owner.prepare_goal_launch_readiness(operation_id, intent)
            }
            AgentDispatchRequest::Resume(operation_id, target) => {
                owner.prepare_resume_readiness(operation_id, target)
            }
            AgentDispatchRequest::RepairResume(operation_id, target, revision) => {
                owner.prepare_current_integration_resume_readiness(operation_id, target, *revision)
            }
            AgentDispatchRequest::Inventory(_)
            | AgentDispatchRequest::WorkspaceObservation(_)
            | AgentDispatchRequest::Diagnose(_, _)
            | AgentDispatchRequest::PlanRestart(_, _)
            | AgentDispatchRequest::Restart(_, _, _, _) => {
                unreachable!("handled before readiness")
            }
        })?;
    let goal_worker_profile = match request {
        AgentDispatchRequest::Goal(_, intent) => Some(
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })?
                .goal_worker_profile(intent)?,
        ),
        _ => None,
    };
    run_agent_readiness(agent, preflight.as_ref())?;
    let reserved_goal = match request {
        AgentDispatchRequest::Goal(operation_id, intent) => Some(
            reserve_goal_supervisor_run(
                supervisor,
                operation_id,
                intent,
                resolve_goal_artifact_repository(supervisor, scope, operation_id, intent)?,
                goal_worker_profile
                    .clone()
                    .expect("Goal request resolved its worker profile"),
            )?
            .supervisor_run_id,
        ),
        _ => None,
    };
    let admission_result =
        agent
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
            .and_then(|mut owner| match request {
                AgentDispatchRequest::Launch(operation_id, intent) => {
                    owner.launch_after_readiness(operation_id, intent, scope, preflight.as_ref())
                }
                AgentDispatchRequest::Goal(operation_id, intent) => owner
                    .launch_goal_after_readiness(operation_id, intent, scope, preflight.as_ref()),
                AgentDispatchRequest::Resume(operation_id, target) => owner
                    .resume_exact_after_readiness(operation_id, target, scope, preflight.as_ref()),
                AgentDispatchRequest::RepairResume(operation_id, target, revision) => owner
                    .resume_with_current_integration_after_readiness(
                        operation_id,
                        target,
                        *revision,
                        scope,
                        preflight.as_ref(),
                    ),
                AgentDispatchRequest::Inventory(_)
                | AgentDispatchRequest::WorkspaceObservation(_)
                | AgentDispatchRequest::Diagnose(_, _)
                | AgentDispatchRequest::PlanRestart(_, _)
                | AgentDispatchRequest::Restart(_, _, _, _) => {
                    unreachable!("handled before readiness")
                }
            });
    let admission = match admission_result {
        Ok(admission) => admission,
        Err(error) => {
            if let AgentDispatchRequest::Goal(operation_id, _) = request
                && error.code != ErrorCode::OwnershipUnknown
                && let Ok(runtime) = supervisor.lock()
            {
                let _ = runtime.fail_reserved_goal(
                    operation_id,
                    "Agent admission failed before Goal promotion".into(),
                    chrono::Utc::now(),
                );
            }
            return Err(error);
        }
    };
    if let AgentDispatchRequest::Goal(operation_id, _) = request
        && let Err(error) = bind_goal_supervisor_run(supervisor, operation_id, &admission.runtime)
    {
        // The Goal reservation and Agent admission are both durable. Returning
        // their accepted identities is safer than turning a post-spawn storage
        // error into a client retry that launches another Agent. Startup and
        // Agent-observer reconciliation retry the exact operation fence.
        ErrorLog::record(&format!("Goal promotion deferred: {}", error.message));
        if let Err(reconcile) = reconcile_pending_supervisor_promotions(supervisor, agent) {
            ErrorLog::record(&format!(
                "Goal promotion reconciliation deferred: {reconcile}"
            ));
        }
    }
    Ok(AgentDispatchAdmission {
        admission,
        supervisor_run_id: reserved_goal,
    })
}

pub(super) struct AgentDispatchAdmission {
    admission: usagi_daemon::usecase::agent_ipc::AgentAdmission,
    supervisor_run_id: Option<usagi_core::domain::supervisor::SupervisorRunId>,
}

pub(super) fn goal_supervisor_caller(workspace: WorkspaceId) -> String {
    format!("goal-composer:{workspace}")
}

pub(super) fn reserve_goal_supervisor_run(
    supervisor: &SharedSupervisorRuntime,
    operation_id: &str,
    intent: &usagi_core::infrastructure::client::AgentGoalIntent,
    artifact_repository: usagi_core::domain::pr_inventory::GitHubRepository,
    worker_profile_id: AgentProfileId,
) -> Result<
    usagi_core::domain::supervisor::SupervisorRunQuery,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    use chrono::Utc;
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    supervisor
        .lock()
        .map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
        })?
        .reserve_goal_for_workspace_with_profile(
            &goal_supervisor_caller(intent.workspace),
            intent.workspace,
            operation_id,
            usagi_daemon::usecase::supervisor_runtime::GoalSpecification::new(
                intent.goal.clone(),
                artifact_repository,
            ),
            worker_profile_id,
            usagi_core::infrastructure::ipc::agent_operation_digest(
                &usagi_core::infrastructure::client::agent_goal_semantic_key(intent),
            ),
            Some("standard".into()),
            Utc::now(),
        )
        .map_err(supervisor_error)
}

pub(super) fn resolve_goal_artifact_repository(
    supervisor: &SharedSupervisorRuntime,
    scope: &dyn SessionScopeResolver,
    operation_id: &str,
    intent: &usagi_core::infrastructure::client::AgentGoalIntent,
) -> Result<
    usagi_core::domain::pr_inventory::GitHubRepository,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    if let Some(repository) = supervisor
        .lock()
        .map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
        })?
        .reserved_goal_repository(operation_id)
        .map_err(supervisor_error)?
    {
        return Ok(repository);
    }
    let resolved = scope
        .resolve_available_scope(intent.workspace, None)
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "Goal workspace is unavailable"))?;
    usagi_daemon::usecase::goal_artifact::resolve_artifact_repository(
        &mut GhProcess,
        &resolved.working_directory,
    )
    .map_err(|_| {
        ProtocolError::new(
            ErrorCode::Unavailable,
            "Goal workspace GitHub repository is unavailable",
        )
    })
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=goal_supervisor_promotion_maps_a_poisoned_owner_to_unavailable
pub(super) fn bind_goal_supervisor_run(
    supervisor: &SharedSupervisorRuntime,
    operation_id: &str,
    worker: &usagi_core::domain::id::AgentRuntimeRef,
) -> Result<
    usagi_core::domain::supervisor::SupervisorRunQuery,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    supervisor
        .lock()
        .map_err(|_| {
            usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                "supervisor runtime is unavailable",
            )
        })?
        .bind_reserved_workspace_root_dispatch(operation_id, worker, chrono::Utc::now())
        .map_err(supervisor_error)
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=delegated_dispatch_is_reserved_before_spawn_and_reconciled_by_exact_operation
pub(super) fn bind_delegated_supervisor_dispatch(
    supervisor: &SharedSupervisorRuntime,
    operation_id: &str,
    worker: &usagi_core::domain::id::AgentRuntimeRef,
) -> anyhow::Result<()> {
    let bound = supervisor
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor runtime is unavailable"))?
        .bind_reserved_delegated_dispatch(operation_id, worker, chrono::Utc::now())?;
    if bound.is_none() {
        anyhow::bail!("delegated supervisor reservation does not exist");
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn start_goal_supervisor_run(
    supervisor: &SharedSupervisorRuntime,
    operation_id: &str,
    intent: &usagi_core::infrastructure::client::AgentGoalIntent,
    worker: &usagi_core::domain::id::AgentRuntimeRef,
) -> Result<
    usagi_core::domain::supervisor::SupervisorRunQuery,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    reserve_goal_supervisor_run(
        supervisor,
        operation_id,
        intent,
        usagi_core::domain::pr_inventory::GitHubRepository::from_name_with_owner("acme/repo")
            .expect("test repository is valid"),
        intent
            .profile
            .clone()
            .unwrap_or_else(|| AgentProfileId::new("claude").unwrap()),
    )?;
    bind_goal_supervisor_run(supervisor, operation_id, worker)
}

#[allow(clippy::too_many_arguments)] // Recovery compares every independently persisted Agent and Supervisor fence.
pub(super) fn promotion_admission_matches(
    agent: &AgentRuntime,
    operation_id: &str,
    worker: &AgentRuntimeRef,
    workspace_id: WorkspaceId,
    requires_worker_session: bool,
    worker_session_id: Option<SessionId>,
    worker_runtime_id: Option<usagi_core::domain::id::AgentRuntimeId>,
    worker_agent_id: Option<usagi_core::domain::id::AgentId>,
    worker_profile_id: Option<&AgentProfileId>,
    worker_semantic_digest: Option<&str>,
) -> anyhow::Result<bool> {
    let worker_session_matches = if requires_worker_session {
        worker_session_id.map_or_else(
            || worker.session_id.is_some() && worker.terminal.session_id.is_some(),
            |expected| {
                worker.session_id == Some(expected) && worker.terminal.session_id == Some(expected)
            },
        )
    } else {
        worker.session_id.is_none() && worker.terminal.session_id.is_none()
    };
    if worker.terminal.workspace_id != workspace_id
        || !worker_session_matches
        || worker_runtime_id.is_some_and(|expected| expected != worker.agent_runtime_id)
    {
        return Ok(false);
    }
    let operation = usagi_core::domain::id::OperationId::parse(operation_id)
        .map_err(|_| anyhow::anyhow!("pending Supervisor operation is invalid"))?;
    let dispatch = agent
        .dispatch_store()
        .run(operation)?
        .ok_or_else(|| anyhow::anyhow!("pending Supervisor dispatch is unavailable"))?;
    let dispatch_agent = agent
        .dispatch_store()
        .agent_in_workspace(workspace_id, dispatch.agent_id)?
        .ok_or_else(|| anyhow::anyhow!("pending Supervisor Agent is unavailable"))?;
    let agent_session_matches = if requires_worker_session {
        worker_session_id.map_or(dispatch_agent.session_id.is_some(), |expected| {
            dispatch_agent.session_id == Some(expected)
        })
    } else {
        dispatch_agent.session_id.is_none()
    };
    if !agent_session_matches
        || worker_agent_id.is_some_and(|expected| dispatch.agent_id != expected)
        || worker_profile_id.is_some_and(|expected| &dispatch_agent.runtime != expected)
    {
        return Ok(false);
    }
    if let Some(expected) = worker_semantic_digest {
        let admission = agent
            .dispatch_store()
            .admission(operation)?
            .ok_or_else(|| anyhow::anyhow!("pending Supervisor admission is unavailable"))?;
        if usagi_core::infrastructure::ipc::agent_operation_digest(&admission.semantic_key)
            != expected
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)] // Root and delegated reservations share one best-effort reconciliation pass.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
pub(super) fn reconcile_pending_supervisor_promotions(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
) -> anyhow::Result<usize> {
    reconcile_supervisor_promotions(supervisor, agent, false)
}

pub(super) fn reconcile_startup_supervisor_promotions(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
) -> anyhow::Result<usize> {
    // Agent hydration is complete and sockets are not accepting requests yet,
    // so a missing outcome is a definitive pre-admission crash residue.
    reconcile_supervisor_promotions(supervisor, agent, true)
}

pub(super) enum PendingPromotionKind {
    Caller { start_operation_id: String },
    Goal,
    Delegated,
}

pub(super) struct PendingPromotionCandidate {
    pub(super) kind: PendingPromotionKind,
    pub(super) operation_id: String,
    pub(super) workspace_id: WorkspaceId,
    pub(super) requires_worker_session: bool,
    pub(super) worker_session_id: Option<SessionId>,
    pub(super) worker_runtime_id: Option<usagi_core::domain::id::AgentRuntimeId>,
    pub(super) worker_agent_id: Option<usagi_core::domain::id::AgentId>,
    pub(super) worker_profile_id: Option<AgentProfileId>,
    pub(super) worker_semantic_digest: Option<String>,
}

pub(super) fn lock_supervisor_runtime(
    supervisor: &SharedSupervisorRuntime,
) -> anyhow::Result<MutexGuard<'_, SupervisorRuntime>> {
    supervisor
        .lock()
        .map_err(|_| anyhow::anyhow!("supervisor runtime is unavailable"))
}

pub(super) fn lock_agent_runtime(
    agent: &SharedAgentRuntime,
) -> anyhow::Result<MutexGuard<'_, AgentRuntime>> {
    agent
        .owner
        .lock()
        .map_err(|_| anyhow::anyhow!("agent owner is unavailable"))
}

pub(super) fn record_supervisor_promotion_result(
    reconciled: &mut usize,
    first_failure: &mut Option<anyhow::Error>,
    operation_id: &str,
    result: anyhow::Result<bool>,
) {
    match result {
        Ok(true) => *reconciled += 1,
        Err(error) if first_failure.is_none() => {
            *first_failure = Some(anyhow::anyhow!(
                "Supervisor promotion {operation_id} remains pending: {error}"
            ));
        }
        Ok(false) | Err(_) => {}
    }
}

pub(super) fn finish_supervisor_promotion_reconciliation(
    reconciled: usize,
    first_failure: Option<anyhow::Error>,
) -> anyhow::Result<usize> {
    match first_failure {
        Some(error) => Err(error),
        None => Ok(reconciled),
    }
}

pub(super) fn reconcile_supervisor_promotions(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
    absence_is_final: bool,
) -> anyhow::Result<usize> {
    let runtime = lock_supervisor_runtime(supervisor)?;
    let pending_callers = runtime.pending_caller_promotions()?;
    let pending_goals = runtime.pending_goal_promotions()?;
    let pending_delegated = runtime.pending_delegated_promotions()?;
    drop(runtime);

    let mut candidates =
        Vec::with_capacity(pending_callers.len() + pending_goals.len() + pending_delegated.len());
    for item in pending_callers {
        candidates.push(PendingPromotionCandidate {
            kind: PendingPromotionKind::Caller {
                start_operation_id: item.start_operation_id,
            },
            operation_id: item.dispatch_operation_id,
            workspace_id: item.workspace_id,
            requires_worker_session: item.worker_session_id.is_some(),
            worker_session_id: item.worker_session_id,
            worker_runtime_id: Some(item.worker_runtime_id),
            worker_agent_id: Some(item.worker_agent_id),
            worker_profile_id: Some(item.worker_profile_id),
            worker_semantic_digest: Some(item.worker_semantic_digest),
        });
    }
    for item in pending_goals {
        candidates.push(PendingPromotionCandidate {
            kind: PendingPromotionKind::Goal,
            operation_id: item.operation_id,
            workspace_id: item.workspace_id,
            requires_worker_session: false,
            worker_session_id: None,
            worker_runtime_id: None,
            worker_agent_id: None,
            worker_profile_id: item.worker_profile_id,
            worker_semantic_digest: item.worker_semantic_digest,
        });
    }
    for item in pending_delegated {
        candidates.push(PendingPromotionCandidate {
            kind: PendingPromotionKind::Delegated,
            operation_id: item.operation_id,
            workspace_id: item.workspace_id,
            requires_worker_session: true,
            worker_session_id: item.worker_session_id,
            worker_runtime_id: None,
            worker_agent_id: item.worker_agent_id,
            worker_profile_id: item.worker_profile_id,
            worker_semantic_digest: item.worker_semantic_digest,
        });
    }

    let mut reconciled = 0;
    let mut first_failure = None;
    for candidate in candidates {
        let result = reconcile_supervisor_promotion(
            supervisor,
            agent,
            &candidate,
            absence_is_final,
            chrono::Utc::now(),
        );
        record_supervisor_promotion_result(
            &mut reconciled,
            &mut first_failure,
            &candidate.operation_id,
            result,
        );
    }
    finish_supervisor_promotion_reconciliation(reconciled, first_failure)
}

pub(super) fn reconcile_supervisor_promotion(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
    candidate: &PendingPromotionCandidate,
    absence_is_final: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<bool> {
    let agent_runtime = lock_agent_runtime(agent)?;
    let outcome = agent_runtime.operation_outcome(&candidate.operation_id);
    drop(agent_runtime);

    reconcile_supervisor_promotion_outcome(
        supervisor,
        agent,
        candidate,
        absence_is_final,
        now,
        outcome,
    )
}

#[allow(clippy::too_many_lines)] // The three durable promotion kinds share one explicit outcome state machine.
pub(super) fn reconcile_supervisor_promotion_outcome(
    supervisor: &SharedSupervisorRuntime,
    agent: &SharedAgentRuntime,
    candidate: &PendingPromotionCandidate,
    absence_is_final: bool,
    now: chrono::DateTime<chrono::Utc>,
    outcome: Option<Result<AgentAdmission, usagi_core::infrastructure::ipc::ProtocolError>>,
) -> anyhow::Result<bool> {
    match outcome {
        Some(Ok(admission)) => {
            let agent_runtime = lock_agent_runtime(agent)?;
            let matches = promotion_admission_matches(
                &agent_runtime,
                &candidate.operation_id,
                &admission.runtime,
                candidate.workspace_id,
                candidate.requires_worker_session,
                candidate.worker_session_id,
                candidate.worker_runtime_id,
                candidate.worker_agent_id,
                candidate.worker_profile_id.as_ref(),
                candidate.worker_semantic_digest.as_deref(),
            )?;
            drop(agent_runtime);

            match (&candidate.kind, matches) {
                (PendingPromotionKind::Caller { start_operation_id }, true) => {
                    let operation =
                        usagi_core::domain::id::OperationId::parse(&candidate.operation_id)
                            .expect("promotion admission matching validated the operation ID");
                    let runtime = lock_supervisor_runtime(supervisor)?;
                    runtime.bind_reserved_caller_dispatch(
                        start_operation_id,
                        operation,
                        &admission.runtime,
                        now,
                    )?;
                }
                (PendingPromotionKind::Goal, true) => {
                    let runtime = lock_supervisor_runtime(supervisor)?;
                    runtime.bind_reserved_workspace_root_dispatch(
                        &candidate.operation_id,
                        &admission.runtime,
                        now,
                    )?;
                }
                (PendingPromotionKind::Delegated, true) => {
                    bind_delegated_supervisor_dispatch(
                        supervisor,
                        &candidate.operation_id,
                        &admission.runtime,
                    )?;
                }
                (PendingPromotionKind::Caller { start_operation_id }, false) => {
                    let runtime = lock_supervisor_runtime(supervisor)?;
                    runtime.fail_reserved_caller_dispatch(
                        start_operation_id,
                        "Agent operation conflicted with its caller-root promotion".into(),
                        now,
                    )?;
                }
                (PendingPromotionKind::Goal, false) => {
                    let runtime = lock_supervisor_runtime(supervisor)?;
                    runtime.fail_reserved_goal(
                        &candidate.operation_id,
                        "Agent operation conflicted with its Goal promotion".into(),
                        now,
                    )?;
                }
                (PendingPromotionKind::Delegated, false) => {
                    let runtime = lock_supervisor_runtime(supervisor)?;
                    runtime.fail_reserved_delegated_dispatch(&candidate.operation_id, now)?;
                }
            }
            Ok(true)
        }
        Some(Err(error)) => {
            if !absence_is_final
                && error.code == usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown
            {
                return Ok(false);
            }
            let runtime = lock_supervisor_runtime(supervisor)?;
            match &candidate.kind {
                PendingPromotionKind::Caller { start_operation_id } => {
                    runtime.fail_reserved_caller_dispatch(
                        start_operation_id,
                        "Agent admission failed before caller-root promotion".into(),
                        now,
                    )?;
                }
                PendingPromotionKind::Goal => {
                    let reason = if error.code
                        == usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown
                    {
                        "Agent admission was not durably recorded before Goal promotion"
                    } else {
                        "Agent admission failed before Goal promotion"
                    };
                    runtime.fail_reserved_goal(&candidate.operation_id, reason.into(), now)?;
                }
                PendingPromotionKind::Delegated => {
                    runtime.fail_reserved_delegated_dispatch(&candidate.operation_id, now)?;
                }
            }
            Ok(true)
        }
        None => {
            if !absence_is_final {
                return Ok(false);
            }
            let runtime = lock_supervisor_runtime(supervisor)?;
            match &candidate.kind {
                PendingPromotionKind::Caller { start_operation_id } => {
                    runtime.fail_reserved_caller_dispatch(
                        start_operation_id,
                        "Agent admission was absent at caller-root startup recovery".into(),
                        now,
                    )?;
                }
                PendingPromotionKind::Goal => {
                    runtime.fail_reserved_goal(
                        &candidate.operation_id,
                        "Agent admission was not durably recorded before Goal promotion".into(),
                        now,
                    )?;
                }
                PendingPromotionKind::Delegated => {
                    runtime.fail_reserved_delegated_dispatch(&candidate.operation_id, now)?;
                }
            }
            Ok(true)
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
pub(super) fn dispatch_agent_maintenance(
    agent: &SharedAgentRuntime,
    request: &AgentDispatchRequest,
    visible_sessions: Option<&BTreeSet<SessionId>>,
) -> Option<Result<serde_json::Value, usagi_core::infrastructure::ipc::ProtocolError>> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    match request {
        AgentDispatchRequest::Inventory(workspace) => Some(
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })
                .map(|agent| {
                    let mut inventory = agent.inventory(*workspace);
                    if let Some(visible_sessions) = visible_sessions {
                        inventory.runtimes.retain(|item| {
                            item.runtime
                                .session_id
                                .is_some_and(|session| visible_sessions.contains(&session))
                        });
                        let runtime_ids = inventory
                            .runtimes
                            .iter()
                            .map(|item| item.runtime.agent_runtime_id)
                            .collect::<BTreeSet<_>>();
                        inventory
                            .resumable
                            .retain(|item| runtime_ids.contains(&item.runtime_id));
                    }
                    serde_json::to_value(inventory).expect("safe Agent inventory is serializable")
                }),
        ),
        AgentDispatchRequest::WorkspaceObservation(workspace) => Some(
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })
                .and_then(|agent| agent.workspace_observation(*workspace))
                .map(|observation| {
                    serde_json::to_value(observation)
                        .expect("safe Agent workspace observation is serializable")
                }),
        ),
        AgentDispatchRequest::Diagnose(workspace, expected) => Some(
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })
                .and_then(|agent| agent.diagnose_integrations(*workspace, expected))
                .map(|diagnosis| {
                    serde_json::to_value(diagnosis).expect("safe Agent diagnosis is serializable")
                }),
        ),
        AgentDispatchRequest::PlanRestart(expected, force) => Some(
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })
                .and_then(|agent| agent.plan_daemon_restart_agents(expected, *force))
                .map(|plan| {
                    serde_json::to_value(plan).expect("safe Agent restart plan is serializable")
                }),
        ),
        AgentDispatchRequest::Restart(workspace, expected, runtimes, force) => Some(
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })
                .and_then(|mut agent| {
                    let (interrupted, diagnosis) =
                        agent.interrupt_outdated_agents(*workspace, expected, runtimes, *force)?;
                    Ok(serde_json::json!({
                        "interrupted": interrupted,
                        "diagnosis": diagnosis,
                        "inventory": agent.inventory(*workspace)
                    }))
                }),
        ),
        AgentDispatchRequest::Launch(..)
        | AgentDispatchRequest::Goal(..)
        | AgentDispatchRequest::Resume(..)
        | AgentDispatchRequest::RepairResume(..) => None,
    }
}

#[allow(clippy::too_many_lines)] // One boundary keeps optional Agent authority and admission atomic.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
pub(super) fn dispatch_agent(
    agent: &SharedAgentRuntime,
    supervisor: &SharedSupervisorRuntime,
    bound: &ConnectionWorkspace,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::client::DaemonRequest;
    use usagi_core::infrastructure::ipc::ResponseOutcome;
    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Agent {
                operation_id,
                intent,
            } => Some((AgentDispatchRequest::Launch(operation_id, intent), None)),
            DaemonRequest::AgentGoal {
                operation_id,
                intent,
            } => Some((AgentDispatchRequest::Goal(operation_id, intent), None)),
            DaemonRequest::AgentInventory {
                workspace,
                caller_context,
            } => Some((AgentDispatchRequest::Inventory(workspace), caller_context)),
            DaemonRequest::AgentWorkspaceObservation { workspace } => {
                Some((AgentDispatchRequest::WorkspaceObservation(workspace), None))
            }
            DaemonRequest::DiagnoseAgents {
                workspace,
                expected,
            } => Some((AgentDispatchRequest::Diagnose(workspace, expected), None)),
            DaemonRequest::PlanDaemonRestartAgents { expected, force } => {
                Some((AgentDispatchRequest::PlanRestart(expected, force), None))
            }
            DaemonRequest::RestartAgents {
                workspace,
                expected,
                runtimes,
                force,
            } => Some((
                AgentDispatchRequest::Restart(workspace, expected, runtimes, force),
                None,
            )),
            DaemonRequest::ResumeAgent {
                operation_id,
                target,
                caller_context,
            } => Some((
                AgentDispatchRequest::Resume(operation_id, target),
                caller_context,
            )),
            DaemonRequest::ResumeAgentWithCurrentIntegration {
                operation_id,
                target,
                expected_revision,
            } => Some((
                AgentDispatchRequest::RepairResume(operation_id, target, expected_revision),
                None,
            )),
            _ => None,
        });
    let Some((request, caller_context)) = request else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let ownership = caller_context.map(|caller_context| {
        use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

        let authenticated = agent
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
            .mcp_dispatch_context(&caller_context.credential)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "agent caller provenance is unknown",
                )
            })?;
        let workspace = bound
            .sessions()
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
            })?
            .workspace_id()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "workspace identity is unavailable")
            })?;
        if authenticated.workspace_id != workspace {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent caller does not belong to this workspace",
            ));
        }
        let requested_workspace = match &request {
            AgentDispatchRequest::Inventory(requested) => *requested,
            AgentDispatchRequest::Resume(_, target) => target.workspace_id,
            _ => workspace,
        };
        if requested_workspace != workspace {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent request does not belong to this workspace",
            ));
        }
        bound
            .sessions()
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
            })?
            .created_session_ids(&authenticated.caller)
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session ownership is unavailable")
            })
    });
    let ownership = match ownership.transpose() {
        Ok(ownership) => ownership,
        Err(error) => {
            return envelope(
                hello,
                request_id,
                ResponseOutcome::Error(error),
                serde_json::Value::Null,
            );
        }
    };
    if let (Some(owned), AgentDispatchRequest::Resume(_, target)) = (&ownership, &request)
        && target
            .session_id
            .is_none_or(|session| !owned.contains(&session))
    {
        return envelope(
            hello,
            request_id,
            ResponseOutcome::Error(usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::PermissionDenied,
                "caller did not create the target session",
            )),
            serde_json::Value::Null,
        );
    }
    let scope = bound.scope_resolver();
    if let Some(result) = dispatch_agent_maintenance(agent, &request, ownership.as_ref()) {
        return match result {
            Ok(body) => envelope(hello, request_id, ResponseOutcome::Ok, body),
            Err(error) => envelope(
                hello,
                request_id,
                ResponseOutcome::Error(error),
                serde_json::Value::Null,
            ),
        };
    }
    // The first owner visit captures immutable facts, the provider command runs
    // after its guard is dropped, and the second visit repeats every fence.
    let result = admit_agent_dispatch_request(agent, supervisor, &scope, &request);
    match result {
        Ok(result) => {
            let supervisor_run_id = result.supervisor_run_id;
            let admission = result.admission;
            // `Ok` is the durable final — direct or replayed after a reconnect —
            // and `ResponseOutcome::Ok` carries no envelope operation identity, so
            // the body is what makes the final correlatable to the producer's
            // pending operation. Every answer therefore states its own
            // `operation_id` and the digest of the intent it was admitted for
            // (#522); the client refuses a final that does not match both.
            let outcome = if admission.completed {
                ResponseOutcome::Ok
            } else {
                ResponseOutcome::Accepted {
                    operation_id: usagi_core::infrastructure::ipc::OperationId(
                        admission.operation_id.clone(),
                    ),
                    operation_revision: admission.revision,
                }
            };
            let mut body = serde_json::json!({
                "operation_id": admission.operation_id,
                "semantic_digest": admission.semantic_digest,
                "terminal": admission.terminal,
                "continuation": admission.continuation,
                "resume_relation": admission.resume_relation,
                "completed": admission.completed,
            });
            if let Some(supervisor_run_id) = supervisor_run_id {
                body["supervisor_run_id"] = serde_json::json!(supervisor_run_id);
            }
            envelope(hello, request_id, outcome, body)
        }
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
pub(super) fn run_agent_readiness(
    agent: &SharedAgentRuntime,
    preflight: Option<&AgentReadinessPreflight>,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    let Some(preflight) = preflight else {
        return Ok(());
    };
    match agent.readiness.observe(preflight.product()) {
        AgentReadiness::Ready => Ok(()),
        AgentReadiness::Unavailable => Err(usagi_core::infrastructure::ipc::ProtocolError::new(
            usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
            "agent CLI is unavailable or not authenticated; install it and sign in, then retry",
        )),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
pub(super) fn dispatch_agent_after_preflight(
    agent: &SharedAgentRuntime,
    operation_id: &str,
    intent: &usagi_core::infrastructure::client::DispatchIntent,
    session: SessionId,
    scope: &dyn SessionScopeResolver,
    planned_worker: Option<&usagi_core::domain::agent::Agent>,
) -> Result<
    usagi_daemon::usecase::agent_ipc::AgentAdmission,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let preflight = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
        .prepare_dispatch_readiness(operation_id, intent)?;
    run_agent_readiness(agent, preflight.as_ref())?;
    let mut agent = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?;
    match planned_worker {
        Some(worker) => agent.dispatch_planned_after_readiness(
            operation_id,
            intent,
            session,
            scope,
            preflight.as_ref(),
            worker,
        ),
        None => {
            agent.dispatch_after_readiness(operation_id, intent, session, scope, preflight.as_ref())
        }
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_hook_capture_works_without_an_inherited_credential
pub(super) fn dispatch_codex_session_capture(
    agent: &SharedAgentRuntime,
    peer_process: &PeerProcess,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::CodexSessionCapture {
                native_session_id,
                caller_context,
            } => Some((native_session_id, caller_context)),
            _ => None,
        });
    let Some((native_session_id, caller_context)) = request else {
        return usagi_daemon::presentation::ipc::reject_unhandled_request(
            request_id,
            body.clone(),
            hello,
        );
    };
    let result = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|mut agent| {
            let credential = caller_context
                .as_ref()
                .map(|context| context.credential.as_str())
                .filter(|credential| !credential.is_empty())
                .or_else(|| {
                    let (parent_pid, process_group) = peer_process.lineage?;
                    agent.hook_credential(peer_process.pid, parent_pid, process_group)
                })
                .map(str::to_owned)
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::OwnershipUnknown,
                        "Codex hook process does not belong to a live Agent runtime",
                    )
                })?;
            agent.capture_codex_session(&credential, native_session_id)
        });
    match result {
        Ok(()) => envelope(
            hello,
            request_id,
            ResponseOutcome::Ok,
            serde_json::Value::Null,
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

/// Routes one private agent lifecycle phase report to the Agent owner.
///
/// Unlike the generic fallback dispatch, a body which is not a well formed phase
/// report is refused here: an agent-originated report must fail closed instead
/// of being echoed back as a success.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_ipc_agent_phase_report_without_a_live_credential_fails_closed
pub(super) fn dispatch_agent_phase_report(
    agent: &SharedAgentRuntime,
    peer_process: &PeerProcess,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::AgentPhaseReport {
                phase,
                caller_context,
            } => Some((phase, caller_context)),
            _ => None,
        });
    let result = request
        .ok_or_else(|| {
            ProtocolError::new(ErrorCode::InvalidArgument, "agent phase report is invalid")
        })
        .and_then(|(phase, caller_context)| {
            let mut agent = agent.lock().map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
            })?;
            let credential = caller_context
                .as_ref()
                .map(|context| context.credential.as_str())
                .filter(|credential| !credential.is_empty())
                .or_else(|| {
                    let (parent_pid, process_group) = peer_process.lineage?;
                    agent.hook_credential(peer_process.pid, parent_pid, process_group)
                })
                .map(str::to_owned)
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::OwnershipUnknown,
                        "phase hook process does not belong to a live Agent runtime",
                    )
                })?;
            agent.report_agent_phase(&credential, phase)
        });
    let outcome = match result {
        Ok(()) => ResponseOutcome::Ok,
        Err(error) => ResponseOutcome::Error(error),
    };
    envelope(hello, request_id, outcome, serde_json::Value::Null)
}

pub(super) fn envelope(
    hello: &usagi_core::infrastructure::ipc::ServerHello,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    outcome: usagi_core::infrastructure::ipc::ResponseOutcome,
    body: serde_json::Value,
) -> usagi_core::infrastructure::ipc::Envelope {
    usagi_core::infrastructure::ipc::Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: usagi_core::infrastructure::ipc::EnvelopeKind::Response {
            request_id,
            outcome,
            body,
        },
    }
}

pub(super) const fn unexpected_daemon_error(code: ErrorCode) -> bool {
    match code {
        ErrorCode::ProtocolMismatch
        | ErrorCode::CapabilityMissing
        | ErrorCode::GenerationMismatch
        | ErrorCode::Unauthenticated
        | ErrorCode::PermissionDenied
        | ErrorCode::ResourceExhausted
        | ErrorCode::Backpressure
        | ErrorCode::DeadlineExceeded
        | ErrorCode::OwnershipUnknown
        | ErrorCode::Unavailable
        | ErrorCode::Internal => true,
        ErrorCode::InvalidArgument
        | ErrorCode::NotFound
        | ErrorCode::StaleTarget
        | ErrorCode::GenerationRolledOver
        | ErrorCode::RevisionConflict
        | ErrorCode::IdempotencyConflict
        | ErrorCode::IdempotencyExpired
        | ErrorCode::SequenceGap
        | ErrorCode::Busy
        | ErrorCode::Cancelled
        | ErrorCode::ResyncRequired => false,
    }
}

pub(super) const fn expected_client_disconnect(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::UnexpectedEof
    )
}

pub(super) fn daemon_request_surface(body: &serde_json::Value) -> &'static str {
    match body.get("kind").and_then(serde_json::Value::as_str) {
        Some("mcp_child_claim") => "mcp_child_claim",
        Some("rollover") => "rollover",
        Some("tenant") => "tenant",
        Some("session") => "session",
        Some(
            "agent"
            | "agent_goal"
            | "agent_inventory"
            | "agent_workspace_observation"
            | "diagnose_agents"
            | "plan_daemon_restart_agents"
            | "restart_agents"
            | "resume_agent"
            | "resume_agent_with_current_integration",
        ) => "agent",
        Some("codex_session_capture") => "codex_session_capture",
        Some("agent_phase_report") => "agent_phase_report",
        Some("dispatch") => "dispatch",
        Some("metrics") => "metrics",
        Some("pr" | "pr_batch" | "pr_dismiss") => "pr",
        Some("dispatch_tool") => "dispatch_tool",
        Some("supervisor_tool") => "supervisor_tool",
        Some("supervisor_snapshot") => "supervisor_snapshot",
        Some("supervisor_control") => "supervisor_control",
        Some("user_decision") => "user_decision",
        Some("terminal") => "terminal",
        Some(_) => "generic",
        None => "unknown",
    }
}

pub(super) fn safe_log_token(value: &str) -> String {
    value
        .bytes()
        .take(128)
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b':' => char::from(byte),
            _ => '_',
        })
        .collect()
}

pub(super) fn unexpected_daemon_response_entry(
    surface: &str,
    response: &Envelope,
) -> Option<String> {
    let EnvelopeKind::Response {
        request_id,
        outcome: ResponseOutcome::Error(error),
        ..
    } = &response.kind
    else {
        return None;
    };
    if !unexpected_daemon_error(error.code) {
        return None;
    }
    let request_id = safe_log_token(&request_id.0);
    let error_id = safe_log_token(&error.error_id);
    Some(format!(
        "daemon request failed: surface={surface} request={} code={:?} retry={:?} side_effect={:?} error_id={} message={}",
        request_id, error.code, error.retry_mode, error.side_effect, error_id, error.message,
    ))
}

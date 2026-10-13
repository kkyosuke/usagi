//! Composition adapter for daemon request families and response shaping.
//!
//! Socket acceptance, process lifecycle, and worker ownership stay in the parent
//! module. This boundary binds already-admitted IPC requests to the injected
//! daemon runtimes and stores.

mod scratchpad;
pub(super) mod session;

use session::{AgentDispatchRequest, authorize_delegation, dispatch_session_action};
use usagi_core::domain::agent::{AgentLaunchClient, AgentLaunchEntry, AgentLaunchSource};
use usagi_daemon::usecase::agent_ipc::AgentLaunchContext;

/// Metadata from the admitted connection; presentation labels confer no authority.
pub(super) fn launch_context(
    source: AgentLaunchSource,
    entrypoint: AgentLaunchEntry,
    client: Option<&AgentLaunchClient>,
    caller: Option<&usagi_core::domain::agent::CallerRef>,
    caller_operation_id: Option<usagi_core::domain::id::OperationId>,
) -> AgentLaunchContext {
    AgentLaunchContext {
        source,
        entrypoint,
        client: client.cloned(),
        caller: caller.cloned(),
        caller_operation_id,
    }
}

use super::{
    AgentAdmission, AgentReadiness, AgentReadinessPreflight, AmbiguousIssueNumber, Arc, BTreeMap,
    BTreeSet, ConnectionId, ConnectionWorkspace, CurrentLocatorFile, DEFAULT_GENERATION_LIMIT,
    DaemonRequest, Deserialize, DispatchStore, DispatchToolAction, Envelope, EnvelopeKind,
    ErrorCode, ErrorLog, GenerationFence, GenerationRegistry, GenerationRegistryFile,
    INBOX_PAGE_MAX, InboxCursor, MetricsObserver, MetricsSample, OperationId, Ordering, Path,
    PathBuf, PeerProcess, PendingDaemonAgentRestart, ResponseOutcome, SessionId,
    SessionRuntimeError, SessionScopeResolver, SharedAgentRuntime, SharedMetricsBroker,
    SharedPrInventory, SharedProcessResourceSampler, SharedSessionRuntime, SharedTerminalRuntime,
    SystemGit, TeardownSignal, TerminalId, TerminalPipelineMetrics, UnixStandbyProbe,
    UserDecisionStore, WorkspaceId, clear_pending_daemon_agent_restart, current_build,
    observe_generation_process, output_pipeline_counters, paths, perform_compensating_remove,
    perform_create, perform_delegated_create, perform_remove_with_merged_head,
    pr_projection_counters, process_start_identity, recover_rollover,
    restore_pending_daemon_agents, rollover_trigger, validate_owned_directory,
    write_pending_daemon_agent_restart,
};

pub(super) struct DispatchToolContext<'a> {
    pub(super) agent: &'a SharedAgentRuntime,
    pub(super) terminal: &'a SharedTerminalRuntime,
    pub(super) bound: &'a ConnectionWorkspace,
    pub(super) pr_inventory: &'a SharedPrInventory,
    pub(super) decisions: &'a UserDecisionStore,
    pub(super) launch_client: Option<&'a AgentLaunchClient>,
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
                | DispatchToolAction::AgentHandoff
                | DispatchToolAction::AgentPeers
                | DispatchToolAction::AgentMessage
                | DispatchToolAction::AgentMessages
                | DispatchToolAction::AgentMessageAck
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
    use usagi_core::infrastructure::ipc::{DispatchAgentIntent, DispatchIntent};
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
        let launch_provenance = if matches!(
            action,
            DispatchToolAction::AgentPeers
                | DispatchToolAction::SessionGet
                | DispatchToolAction::AgentList
                | DispatchToolAction::AgentGet
        ) {
            runtime.agent_launch_provenance(workspace)?
        } else {
            std::collections::BTreeMap::new()
        };
        drop(runtime);
        if matches!(
            action,
            DispatchToolAction::AgentPeers
                | DispatchToolAction::AgentMessage
                | DispatchToolAction::AgentMessages
                | DispatchToolAction::AgentMessageAck
        ) {
            let mut response = usagi_daemon::usecase::peer_messages::handle(
                &store,
                workspace,
                &caller,
                parent_dispatch_run,
                action,
                payload,
            )?;
            if let Some(peers) = response
                .get_mut("agents")
                .and_then(serde_json::Value::as_array_mut)
            {
                for peer in peers {
                    let id = peer
                        .get("agent_id")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<AgentId>(value).ok());
                    peer["launch_provenance"] =
                        serde_json::json!(id.and_then(|id| launch_provenance.get(&id)));
                }
            }
            if action == DispatchToolAction::AgentMessage
                && let Some(recipient) = response
                    .get("message")
                    .and_then(|message| message.get("to_agent_id"))
                    .cloned()
                    .and_then(|id| serde_json::from_value::<AgentId>(id).ok())
                && let Some(session) = caller.session_id
                && let Ok(mut runtime) = agent.lock()
            {
                let _ = runtime.notify_peer(workspace, session, recipient);
            }
            return Ok((ResponseOutcome::Ok, response));
        }
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
            DispatchToolAction::Dispatch | DispatchToolAction::AgentHandoff => {
                let handoff = action == DispatchToolAction::AgentHandoff;
                let payload = if handoff {
                    usagi_daemon::usecase::peer_messages::handoff_payload(
                        &caller, &snapshot, payload,
                    )?
                } else {
                    payload
                };
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
                if !handoff {
                    bound
                        .sessions()
                        .lock()
                        .map_err(|_| {
                            ProtocolError::new(
                                ErrorCode::Unavailable,
                                "session runtime is unavailable",
                            )
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
                }
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
                let created_body = if handoff {
                    snapshot
                } else {
                    perform_create(
                        bound.sessions(),
                        &SystemGit,
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
                    })?
                    .body
                };
                let (session_id, parent_session_id) =
                    session_lineage_by_name(&created_body, &session_name).ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "created session is not available",
                        )
                    })?;
                if !handoff {
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
                }
                let reserved_worker = if handoff {
                    Some(
                        agent
                            .lock()
                            .map_err(|_| {
                                ProtocolError::new(
                                    ErrorCode::Unavailable,
                                    "agent owner is unavailable",
                                )
                            })?
                            .plan_peer_worker(&operation_id, workspace, &caller, &selected)?,
                    )
                } else {
                    None
                };
                let scope = bound.scope_resolver();
                let dispatch_intent = DispatchIntent {
                    workspace,
                    session_name: session_name.clone(),
                    caller,
                    agent: selected,
                    prompt: input.prompt,
                };
                let admission = dispatch_agent_after_preflight(
                    agent,
                    &operation_id,
                    &dispatch_intent,
                    session_id,
                    &scope,
                    reserved_worker.as_ref(),
                    launch_context(
                        AgentLaunchSource::Mcp,
                        if handoff {
                            AgentLaunchEntry::AgentHandoff
                        } else {
                            AgentLaunchEntry::SessionDispatch
                        },
                        context.launch_client,
                        Some(&dispatch_intent.caller),
                        Some(parent_dispatch_run),
                    ),
                )?;
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
                let agents = store.agents_in_workspace(workspace).map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable"))?.into_iter().filter(|item| item.session_id == Some(session_id)).map(|item| Ok(serde_json::json!({"agent_id": item.agent_id, "runtime": item.runtime, "model": item.model, "status": item.status, "task": task_for(item.agent_id)?, "launch_provenance": launch_provenance.get(&item.agent_id)}))).collect::<Result<Vec<_>, ProtocolError>>()?;
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
                let agents = store.agents_in_workspace(workspace).map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable"))?.into_iter().filter(|item| item.session_id.is_some_and(|id| owned_sessions.contains(&id)) && session.is_none_or(|id| item.session_id == Some(id)) && status.is_none_or(|value| item.status == value)).map(|item| Ok(serde_json::json!({"agent_id": item.agent_id, "session_id": item.session_id, "runtime": item.runtime, "model": item.model, "status": item.status, "task": task_for(item.agent_id)?, "launch_provenance": launch_provenance.get(&item.agent_id)}))).collect::<Result<Vec<_>, ProtocolError>>()?;
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
                    serde_json::json!({"agent": item, "runs": runs, "launch_provenance": launch_provenance.get(&item.agent_id)}),
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

/// PR events are deliberately only hints; the IPC request always returns this
/// durable snapshot so reconnects and dropped events converge without replay.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=pr_snapshot_events_cover_success_scoped_and_lane_errors
pub(super) fn dispatch_pr_snapshot(
    inventory: &SharedPrInventory,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{DaemonRequest, PrAction};
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
        allow_comment: bool,
        #[serde(default)]
        require_confirmation: bool,
        #[serde(default)]
        recommendation: Option<usagi_core::domain::user_decision::UserDecisionRecommendation>,
        #[serde(default)]
        selection_limits: Option<usagi_core::domain::user_decision::UserDecisionSelectionLimits>,
        #[serde(default)]
        selection_mode: usagi_core::domain::user_decision::UserDecisionSelectionMode,
        #[serde(default)]
        context: Vec<usagi_core::domain::user_decision::UserDecisionContext>,
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
                use usagi_core::infrastructure::ipc::TuiUserDecisionAction;
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
                        .create_with_default_expiry(UserDecision {
                            decision_id: UserDecisionId::new(),
                            owner,
                            title: input.title,
                            prompt: input.prompt,
                            options: input.options,
                            allow_freeform: input.allow_freeform,
                            allow_comment: input.allow_comment,
                            require_confirmation: input.require_confirmation,
                            recommendation: input.recommendation,
                            selection_limits: input.selection_limits,
                            selection_mode: input.selection_mode,
                            context: input.context,
                            expires_at: input.expires_at,
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
    launch_client: &AgentLaunchClient,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::DaemonRequest;
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
        if let Some(id) = session_id_by_name(&snapshot, &intent.session_name) {
            return Ok(id);
        }
        let created = perform_create(
            bound.sessions(),
            &SystemGit,
            &operation_id,
            &serde_json::json!({"name": intent.session_name}),
        )
        .map_err(|error| ProtocolError::new(ErrorCode::InvalidArgument, error.safe_message()))?;
        session_id_by_name(&created.body, &intent.session_name).ok_or_else(|| {
            ProtocolError::new(ErrorCode::Unavailable, "created session is not available")
        })
    })();
    let result = session_id.and_then(|session_id| {
        let scope = bound.scope_resolver();
        dispatch_agent_after_preflight(
            agent,
            &operation_id,
            &intent,
            session_id,
            &scope,
            None,
            launch_context(
                AgentLaunchSource::Unknown,
                AgentLaunchEntry::LegacyDispatch,
                Some(launch_client),
                None,
                None,
            ),
        )
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
                        restart_requested: restart_agents.is_some(),
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
    use usagi_core::infrastructure::ipc::{DaemonRequest, MetricsAction};
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
    pub(super) launch_client: Option<&'a AgentLaunchClient>,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_create_reaches_daemon_and_durable_lifecycle
pub(super) fn dispatch_session(
    context: &SessionDispatchContext<'_>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::DaemonRequest;
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
    use usagi_core::infrastructure::ipc::DaemonRequest;
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
    action: usagi_core::infrastructure::ipc::SessionAction,
    result: Result<usagi_daemon::usecase::session_runtime::SessionReply, SessionRuntimeError>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::ResponseOutcome;
    use usagi_core::infrastructure::ipc::SessionAction;
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
                | SessionAction::Agents
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
    inventory: Option<usagi_core::infrastructure::ipc::PrSnapshot>,
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
#[allow(clippy::too_many_lines)] // 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
pub(super) fn clean_orphan_session_resources(
    bound: &ConnectionWorkspace,
    agent: Option<&SharedAgentRuntime>,
    apply: bool,
    force: bool,
    target: Option<&usagi_core::usecase::clean::CleanTarget>,
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
        .filter(|candidate| target.is_none_or(|target| target.matches(candidate)))
        .filter(|candidate| {
            matches!(
                candidate,
                CleanCandidate::Worktree { .. } | CleanCandidate::Branch { .. }
            )
        })
        .collect::<Vec<_>>();
    if target.is_some() && git_candidates.len() != 1 {
        return Err(SessionRuntimeError::DurableFailure(
            "selected resource is no longer an orphan cleanup candidate".into(),
        ));
    }
    let agent = agent.filter(|_| target.is_none());
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
            "target": target,
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
                usagi_core::infrastructure::artifacts::archive(path)
                    .map_err(anyhow::Error::from)
                    .and_then(|_| {
                        remove_worktree(
                            &SystemGit,
                            &root,
                            path,
                            candidate.requires_force() && force,
                        )
                    })
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
        "target": target,
        "candidates": described,
        "removed": removed,
        "protected": protected,
    }))
}

#[coverage(off)]
// coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
fn admit_agent_dispatch_request(
    agent: &SharedAgentRuntime,
    scope: &dyn SessionScopeResolver,
    request: &AgentDispatchRequest,
    resume_caller: Option<&usagi_daemon::usecase::agent_ipc::AuthenticatedDispatchCaller>,
    launch_client: &AgentLaunchClient,
) -> Result<AgentAdmission, usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let preflight = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|owner| match request {
            AgentDispatchRequest::Launch(operation_id, intent) => {
                owner.prepare_launch_readiness(operation_id, intent)
            }
            AgentDispatchRequest::Resume(operation_id, target) => {
                owner.prepare_resume_readiness(operation_id, target)
            }
            AgentDispatchRequest::RepairResume(operation_id, target, revision) => {
                owner.prepare_current_integration_resume_readiness(operation_id, target, *revision)
            }
            _ => unreachable!("maintenance was handled before readiness"),
        })?;
    run_agent_readiness(agent, preflight.as_ref())?;
    let workspace = match request {
        AgentDispatchRequest::Launch(_, intent) => intent.workspace,
        AgentDispatchRequest::Resume(_, target)
        | AgentDispatchRequest::RepairResume(_, target, _) => target.workspace_id,
        _ => unreachable!("maintenance was handled before readiness"),
    };
    let _environment = agent.prepare_environment(workspace, preflight.is_some())?;
    let (resume_source, resume_actor, resume_operation) = match resume_caller {
        Some(authenticated) => (
            AgentLaunchSource::Mcp,
            Some(&authenticated.caller),
            Some(authenticated.run_id),
        ),
        None => (AgentLaunchSource::Manual, None, None),
    };
    agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|mut owner| match request {
            AgentDispatchRequest::Launch(operation_id, intent) => owner
                .launch_from_after_readiness(
                    operation_id,
                    intent,
                    scope,
                    preflight.as_ref(),
                    launch_context(
                        AgentLaunchSource::Manual,
                        AgentLaunchEntry::Agent,
                        Some(launch_client),
                        None,
                        None,
                    ),
                ),
            AgentDispatchRequest::Resume(operation_id, target) => owner
                .resume_from_after_readiness(
                    operation_id,
                    target,
                    scope,
                    preflight.as_ref(),
                    launch_context(
                        resume_source,
                        AgentLaunchEntry::SessionResume,
                        Some(launch_client),
                        resume_actor,
                        resume_operation,
                    ),
                ),
            AgentDispatchRequest::RepairResume(operation_id, target, revision) => owner
                .resume_with_current_integration_from_after_readiness(
                    operation_id,
                    target,
                    *revision,
                    scope,
                    preflight.as_ref(),
                    launch_context(
                        resume_source,
                        AgentLaunchEntry::IntegrationRepair,
                        Some(launch_client),
                        resume_actor,
                        resume_operation,
                    ),
                ),
            _ => unreachable!("maintenance was handled before readiness"),
        })
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
fn dispatch_agent_maintenance(
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
        | AgentDispatchRequest::Resume(..)
        | AgentDispatchRequest::RepairResume(..) => None,
    }
}

#[allow(clippy::too_many_lines)] // One boundary keeps optional Agent authority and admission atomic.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
pub(super) fn dispatch_agent(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
    launch_client: &AgentLaunchClient,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::DaemonRequest;
    use usagi_core::infrastructure::ipc::ResponseOutcome;
    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Agent {
                operation_id,
                intent,
            } => Some((AgentDispatchRequest::Launch(operation_id, intent), None)),
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
    let mut resume_caller = None;
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
        resume_caller = Some(authenticated.clone());
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
    let result = admit_agent_dispatch_request(
        agent,
        &scope,
        &request,
        resume_caller.as_ref(),
        launch_client,
    );
    match result {
        Ok(admission) => {
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
            let body = serde_json::json!({
                "operation_id": admission.operation_id,
                "semantic_digest": admission.semantic_digest,
                "terminal": admission.terminal,
                "continuation": admission.continuation,
                "resume_relation": admission.resume_relation,
                "completed": admission.completed,
            });
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
    intent: &usagi_core::infrastructure::ipc::DispatchIntent,
    session: SessionId,
    scope: &dyn SessionScopeResolver,
    planned_worker: Option<&usagi_core::domain::agent::Agent>,
    context: AgentLaunchContext,
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
    let _environment = agent.prepare_environment(intent.workspace, preflight.is_some())?;
    let mut agent = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?;
    agent.dispatch_from_after_readiness(
        operation_id,
        intent,
        session,
        scope,
        preflight.as_ref(),
        planned_worker,
        context,
    )
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
                native_session_id,
                caller_context,
            } => Some((phase, native_session_id, caller_context)),
            _ => None,
        });
    let result = request
        .ok_or_else(|| {
            ProtocolError::new(ErrorCode::InvalidArgument, "agent phase report is invalid")
        })
        .and_then(|(phase, native_session_id, caller_context)| {
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
            agent.report_agent_phase_with_session(&credential, phase, native_session_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn organization_projection_is_rooted_bounded_and_cycle_safe() {
        let parent = SessionId::new();
        let child = SessionId::new();
        let names = BTreeMap::from([(parent, "parent".to_owned()), (child, "child".to_owned())]);
        let parents = BTreeMap::from([(parent, None), (child, Some(parent))]);

        assert_eq!(
            session::session_organization(child, &names, &parents),
            (
                Some("parent".to_owned()),
                2,
                vec![
                    "Director".to_owned(),
                    "parent".to_owned(),
                    "child".to_owned(),
                ],
            )
        );

        let cycle = BTreeMap::from([(parent, Some(child)), (child, Some(parent))]);
        let (_, depth, path) = session::session_organization(child, &names, &cycle);
        assert_eq!(depth, 2);
        assert_eq!(path.len(), 3);

        let missing = BTreeMap::from([(child, Some(SessionId::new()))]);
        assert_eq!(
            session::session_organization(child, &names, &missing),
            (None, 1, vec!["Director".to_owned(), "child".to_owned()],)
        );
    }
}

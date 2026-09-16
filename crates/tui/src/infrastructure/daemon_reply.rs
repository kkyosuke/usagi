//! daemon の reply を TUI 語彙へ翻訳する adapter。
//!
//! request の組み立て、reply の decode、operation の correlate、typed failure の
//! 射影だけを持つ純関数の層である。接続・lane・thread の所有は合成ルートに残り、
//! ここには持ち込まない。だからこの層は fake の payload だけで単体テストできる。

use usagi_core::domain::id::{SessionId, WorkspaceId};
use usagi_core::domain::session_lifecycle::ManagedSession;
use usagi_core::domain::terminal_launch::TerminalLaunchScope;
use usagi_core::infrastructure::ipc::{
    AgentGoalIntent, AgentLaunchIntent, ClientError, DaemonReply, DaemonRequest, PrSnapshot,
    TerminalRequest, TerminalSnapshotMode,
};
use usagi_core::usecase::vt_screen::ScreenCheckpoint;

use crate::usecase::application::agent_runtime_ports::{AgentPaneAdmission, ExactAgentResume};
use crate::usecase::application::controller::{
    AppEvent, BackendEvent, SafeError, SafeMessage, Target,
};
use crate::usecase::application::pane_runtime::Geometry;
use crate::usecase::application::terminal_session::{
    TerminalAttachScreen, TerminalChunk, TerminalError, TerminalInputOutcome,
};
use crate::usecase::application::work_run_control::{
    WORK_RUN_ACTION_UNCONFIRMED, WorkRunControlError, WorkRunControlResult,
};

/// How many acknowledgements one terminal caches before the oldest is dropped.
/// A bounded cache keeps a daemon that stops acknowledging from growing the
/// client's memory while still resolving the acks that do arrive.
pub const MAX_CACHED_INPUT_ACK_DEPTH: usize = 16;

/// The one safe message every failed Agent launch correlation reports. It says
/// nothing about which check failed, so a mismatched identity, a foreign digest,
/// and an unfenced terminal all read the same on screen.
pub const AGENT_LAUNCH_UNCORRELATED: &str = "agent launch could not be correlated safely";

pub type PrSnapshotResult = Result<PrSnapshot, String>;

pub type PrObservations = Vec<(SessionId, PrSnapshotResult)>;

#[must_use]
pub fn remove_session_payload(
    name: &str,
    force: bool,
    force_delete_branch: bool,
    purge_orphan: bool,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "force": force,
        "force_delete_branch": force_delete_branch,
        "purge_orphan": purge_orphan,
    })
}

#[must_use]
pub fn pr_snapshot_events(
    result: Result<PrObservations, String>,
    sessions: &[SessionId],
) -> Vec<AppEvent> {
    result
        .unwrap_or_else(|error| {
            sessions
                .iter()
                .map(|session| (*session, Err(error.clone())))
                .collect()
        })
        .into_iter()
        .map(|(session, snapshot)| {
            let target = Target::Session(session);
            let snapshot = snapshot.and_then(|snapshot| {
                if snapshot.session_id == session {
                    Ok(snapshot)
                } else {
                    Err("invalid PR snapshot identity".to_owned())
                }
            });
            AppEvent::Backend(match snapshot {
                Ok(snapshot) => BackendEvent::PullRequestsLoaded {
                    target,
                    revision: snapshot.revision,
                    prs: snapshot.entries,
                },
                Err(_) => BackendEvent::PullRequestsError {
                    target,
                    error: SafeError {
                        message: SafeMessage::new("Pull Requests are unavailable."),
                        error_id: "pr-load".to_owned(),
                    },
                },
            })
        })
        .collect()
}

/// The generation a terminal request must be delivered to.
///
/// The destination is read from the typed payload rather than from the action it
/// is sent under, because the `TerminalRef` is what names the daemon that holds
/// the PTY. A request that carries no such reference has no single owner —
/// control work belongs to the active generation and a scope query belongs to
/// all of them — so it is refused here instead of being sent to whichever lane
/// happened to be open.
///
/// # Errors
///
/// Returns a typed terminal failure when the request names no single owner generation.
pub fn owner_of_terminal_request(
    request: &TerminalRequest,
) -> Result<usagi_core::domain::id::DaemonGeneration, TerminalError> {
    use usagi_core::infrastructure::owner_routing::{RouteTarget, route_terminal_request};

    match route_terminal_request(request) {
        RouteTarget::Owner(generation) => Ok(generation),
        RouteTarget::ActiveControl | RouteTarget::EveryGeneration => Err(TerminalError::Stale),
    }
}

/// Maps a typed client failure onto the safe terminal feedback the UI renders.
/// No mapping authorizes a local PTY fallback.
#[must_use]
pub fn map_terminal_error(error: &usagi_core::infrastructure::ipc::ClientError) -> TerminalError {
    use usagi_core::infrastructure::ipc::ErrorCode;
    match error.code() {
        ErrorCode::ResyncRequired => TerminalError::ResyncRequired,
        ErrorCode::StaleTarget => TerminalError::Stale,
        ErrorCode::OwnershipUnknown => TerminalError::Orphaned,
        ErrorCode::IdempotencyExpired | ErrorCode::SequenceGap => TerminalError::OrderingMismatch,
        _ => TerminalError::Unavailable,
    }
}

/// The geometry a terminal snapshot reply reports, when it carries a complete
/// one.
///
/// The daemon answers a resize with the snapshot of the terminal as it now
/// stands, which is the authoritative viewport for every window sharing it. A
/// partial or out-of-range pair is treated as absent so the caller keeps what it
/// asked for rather than decoding at an invented size.
#[must_use]
pub fn reply_geometry(body: &serde_json::Value) -> Option<Geometry> {
    let geometry = &body["geometry"];
    Some(Geometry {
        cols: u16::try_from(geometry["cols"].as_u64()?).ok()?,
        rows: u16::try_from(geometry["rows"].as_u64()?).ok()?,
    })
}

/// Decodes the terminal owner's sequence-consuming input acknowledgement.
///
/// The wire enum is deliberately validated here instead of being collapsed to
/// a body-less success. Unknown variants, malformed partial-write counts and
/// pathological cached nesting all have an unknown effect and therefore fail
/// closed without authorizing a retry.
///
/// # Errors
///
/// Returns a safe message when the reply is not a usable acknowledgement.
pub fn decode_terminal_input_ack(
    body: &serde_json::Value,
    input_len: usize,
) -> Result<TerminalInputOutcome, TerminalError> {
    let object = body
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or(TerminalError::InputEffectUnknown)?;
    let ack = object.get("ack").ok_or(TerminalError::InputEffectUnknown)?;
    decode_terminal_input_ack_value(ack, input_len, 0)
}

///
/// # Errors
///
/// Returns a safe message when the acknowledgement payload is malformed or names an outcome this client cannot act on.
pub fn decode_terminal_input_ack_value(
    ack: &serde_json::Value,
    input_len: usize,
    cached_depth: usize,
) -> Result<TerminalInputOutcome, TerminalError> {
    match ack.as_str() {
        Some("Written") => return Ok(TerminalInputOutcome::Written),
        Some("Failed") => return Ok(TerminalInputOutcome::Failed),
        Some(_) => return Err(TerminalError::InputEffectUnknown),
        None => {}
    }

    let variant = ack
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or(TerminalError::InputEffectUnknown)?;
    if let Some(ambiguous) = variant.get("Ambiguous") {
        let fields = ambiguous
            .as_object()
            .filter(|object| object.len() == 1)
            .ok_or(TerminalError::InputEffectUnknown)?;
        let applied_prefix = fields
            .get("applied_prefix")
            .and_then(serde_json::Value::as_u64)
            .and_then(|prefix| usize::try_from(prefix).ok())
            .filter(|prefix| *prefix > 0 && *prefix <= input_len)
            .ok_or(TerminalError::InputEffectUnknown)?;
        return Ok(TerminalInputOutcome::Ambiguous { applied_prefix });
    }
    if let Some(cached) = variant.get("Cached") {
        if cached_depth >= MAX_CACHED_INPUT_ACK_DEPTH {
            return Err(TerminalError::InputEffectUnknown);
        }
        return decode_terminal_input_ack_value(cached, input_len, cached_depth + 1);
    }
    Err(TerminalError::InputEffectUnknown)
}

///
/// # Errors
///
/// Returns a safe message when the inventory reply is malformed.
pub fn decode_terminal_inventory(
    body: &serde_json::Value,
) -> Result<Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>, TerminalError> {
    body.get("terminals")
        .and_then(serde_json::Value::as_array)
        .ok_or(TerminalError::Unavailable)?
        .iter()
        .map(|item| serde_json::from_value(item.clone()).map_err(|_| TerminalError::Unavailable))
        .collect()
}

/// Decode the screen an attach / resync snapshot carries, according to the
/// contract the connection negotiated.
///
/// On the checkpoint path the frame must satisfy `base_offset == output_offset`
/// (a checkpoint is complete at `output_offset`, so it has no tail) and carry a
/// checkpoint this build accepts; anything else is refused rather than displayed.
/// On the legacy path the retained `replay` tail is **not read at all**: a tail
/// cut mid UTF-8 / CSI / OSC must never reach a parser, so the client fails
/// closed to a history-less view and renders only output after `output_offset`.
///
/// # Errors
///
/// Returns a safe message when the attach frame carries no usable checkpoint.
pub fn decode_attach_screen(
    mode: TerminalSnapshotMode,
    snapshot: &serde_json::Value,
    base_offset: u64,
    output_offset: u64,
) -> Result<TerminalAttachScreen, TerminalError> {
    match mode {
        TerminalSnapshotMode::Checkpoint => {
            if base_offset != output_offset {
                return Err(TerminalError::Unavailable);
            }
            // The frame is already bounded by the negotiated IPC frame limit;
            // the checkpoint's own bounds are enforced when it is restored.
            let checkpoint: ScreenCheckpoint = serde_json::from_value(snapshot["screen"].clone())
                .map_err(|_| TerminalError::Unavailable)?;
            Ok(TerminalAttachScreen::Checkpoint(Box::new(checkpoint)))
        }
        TerminalSnapshotMode::LegacyFailClosed => Ok(TerminalAttachScreen::HistoryUnavailable),
    }
}

#[must_use]
pub fn terminal_inventory_matches_scope(
    entries: &[usagi_core::domain::terminal_launch::TerminalInventoryEntry],
    scope: &TerminalLaunchScope,
) -> bool {
    entries.iter().all(|entry| {
        entry.terminal.workspace_id == scope.workspace_id
            && entry.terminal.session_id == scope.session_id
            && entry.terminal.worktree_id == scope.worktree_id
    })
}

#[must_use]
pub fn agent_inventory_request(workspace: WorkspaceId) -> DaemonRequest {
    DaemonRequest::AgentInventory {
        workspace,
        caller_context: None,
    }
}

///
/// # Errors
///
/// Returns a safe message when the snapshot reply cannot be decoded.
pub fn decode_work_run_snapshot_reply(
    reply: DaemonReply,
) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
    match reply {
        DaemonReply::Ok(body) => serde_json::from_value(body)
            .map_err(|_| "daemon returned invalid Work Run progress".to_owned()),
        DaemonReply::Accepted { .. } => Err("Work Run progress is unavailable".to_owned()),
    }
}

///
/// # Errors
///
/// Returns a typed control failure when the daemon did not answer with a final, typed result.
pub fn decode_work_run_control_reply(
    reply: DaemonReply,
) -> Result<WorkRunControlResult, WorkRunControlError> {
    match reply {
        DaemonReply::Ok(body) => {
            if body.get("state").is_some() {
                serde_json::from_value(body)
                    .map(|run| WorkRunControlResult::Updated(Box::new(run)))
                    .map_err(|_| {
                        WorkRunControlError::Unconfirmed(
                            "daemon returned an invalid Work Run result".to_owned(),
                        )
                    })
            } else {
                serde_json::from_value(body)
                    .map(WorkRunControlResult::Deleted)
                    .map_err(|_| {
                        WorkRunControlError::Unconfirmed(
                            "daemon returned an invalid Work Run deletion".to_owned(),
                        )
                    })
            }
        }
        // A durable Accepted acknowledgement proves admission, not the final
        // aggregate. Treating its optional body as final could make the UI
        // forget the only operation identity safe to replay after ambiguity.
        DaemonReply::Accepted { .. } => Err(WorkRunControlError::Unconfirmed(
            WORK_RUN_ACTION_UNCONFIRMED.to_owned(),
        )),
    }
}

#[must_use]
pub fn work_run_control_client_error(error: ClientError) -> WorkRunControlError {
    match error {
        ClientError::Protocol(protocol)
            if protocol.side_effect == usagi_core::infrastructure::ipc::SideEffect::None =>
        {
            WorkRunControlError::Rejected(protocol.message)
        }
        _ => WorkRunControlError::Unconfirmed(WORK_RUN_ACTION_UNCONFIRMED.to_owned()),
    }
}

#[must_use]
pub fn exact_agent_resume_request(
    operation_id: usagi_core::domain::id::OperationId,
    target: usagi_core::domain::agent::AgentResumeTarget,
) -> DaemonRequest {
    DaemonRequest::ResumeAgent {
        operation_id: operation_id.to_string(),
        target,
        caller_context: None,
    }
}

/// Decode one exact-target resume answer, keeping the daemon's own lineage and
/// source-to-replacement relation. Nothing is inferred here: a body without a
/// decodable relation yields `None` and the TUI refuses the replacement (#510).
///
/// # Errors
///
/// Returns a safe message when the resume payload is not an exact, typed projection.
pub fn decode_exact_agent_resume(body: &serde_json::Value) -> Result<ExactAgentResume, String> {
    let terminal = body
        .get("terminal")
        .cloned()
        .ok_or_else(|| "provider resume returned no terminal".to_owned())
        .and_then(|terminal| {
            serde_json::from_value(terminal)
                .map_err(|_| "provider resume returned an invalid terminal".to_owned())
        })?;
    let continuation = body
        .get("continuation")
        .filter(|value| !value.is_null())
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let relation = body
        .get("resume_relation")
        .filter(|value| !value.is_null())
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    Ok(ExactAgentResume {
        terminal,
        continuation,
        relation,
    })
}

///
/// # Errors
///
/// Returns a safe message when the admission payload does not name the fenced terminal the daemon spawned.
pub fn decode_agent_admission(
    body: &serde_json::Value,
    operation: &str,
) -> Result<AgentPaneAdmission, String> {
    let terminal = body
        .get("terminal")
        .cloned()
        .ok_or_else(|| format!("{operation} returned no terminal"))
        .and_then(|terminal| {
            serde_json::from_value(terminal)
                .map_err(|_| format!("{operation} returned an invalid terminal"))
        })?;
    let continuation = body
        .get("continuation")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| format!("{operation} returned an invalid continuation"))?;
    let supervisor_run_id = body
        .get("supervisor_run_id")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| format!("{operation} returned an invalid Work Run identity"))?;
    Ok(AgentPaneAdmission {
        terminal,
        continuation,
        supervisor_run_id,
    })
}

/// Build the request for one pane's Agent launch. The pending pane's own
/// operation is the wire identity; the adapter never mints another (#522).
#[must_use]
pub fn agent_launch_request(
    operation: usagi_core::domain::id::OperationId,
    intent: AgentLaunchIntent,
) -> DaemonRequest {
    DaemonRequest::Agent {
        operation_id: operation.to_string(),
        intent,
    }
}

#[must_use]
pub fn agent_goal_request(
    operation: usagi_core::domain::id::OperationId,
    intent: AgentGoalIntent,
) -> DaemonRequest {
    DaemonRequest::AgentGoal {
        operation_id: operation.to_string(),
        intent,
    }
}

/// Correlate one Agent launch reply back to the pending operation that issued it.
///
/// The reply is usable only when *every* fence agrees with the request: the
/// admission or final states the same `operation_id`, the digest of the intent it
/// was admitted for matches the one computed here, `completed` matches the reply
/// class (an `Accepted` is running, an `Ok` is the durable final — direct or
/// replayed after a reconnect), the terminal is fenced to the requested scope, and
/// an ordinary launch carries no resume relation. Anything else — a missing or
/// foreign identity, another intent's digest, a `completed: false` offered as a
/// final, a terminal from another scope — is a correlation failure that leaves the
/// pending pane to fail safely instead of promoting a side effect that may belong
/// to another operation (#522).
///
/// # Errors
///
/// Returns a safe message when the launch reply cannot be correlated safely.
pub fn correlate_agent_launch(
    reply: DaemonReply,
    operation: usagi_core::domain::id::OperationId,
    intent: &AgentLaunchIntent,
) -> Result<AgentPaneAdmission, String> {
    let expected_digest = usagi_core::infrastructure::ipc::agent_operation_digest(
        &usagi_core::infrastructure::ipc::agent_launch_semantic_key(intent),
    );
    correlate_agent_response(
        reply,
        operation,
        &expected_digest,
        intent.workspace,
        intent.session,
    )
}

///
/// # Errors
///
/// Returns a safe message when the goal reply cannot be correlated safely.
pub fn correlate_agent_goal(
    reply: DaemonReply,
    operation: usagi_core::domain::id::OperationId,
    intent: &AgentGoalIntent,
) -> Result<AgentPaneAdmission, String> {
    let expected_digest = usagi_core::infrastructure::ipc::agent_operation_digest(
        &usagi_core::infrastructure::ipc::agent_goal_semantic_key(intent),
    );
    correlate_agent_response(reply, operation, &expected_digest, intent.workspace, None)
}

///
/// # Errors
///
/// Returns a safe message when the reply cannot be correlated to the operation this client submitted.
pub fn correlate_agent_response(
    reply: DaemonReply,
    operation: usagi_core::domain::id::OperationId,
    expected_digest: &str,
    workspace: WorkspaceId,
    session: Option<SessionId>,
) -> Result<AgentPaneAdmission, String> {
    let expected = operation.to_string();
    let (body, final_reply) = match reply {
        // `Accepted` proves admission of this operation twice over: the envelope
        // identity the transport matched, and the body identity checked below.
        DaemonReply::Accepted {
            operation_id, body, ..
        } if operation_id == expected => (body, false),
        DaemonReply::Accepted { .. } => return Err(AGENT_LAUNCH_UNCORRELATED.to_owned()),
        // `ResponseOutcome::Ok` carries no envelope operation identity, so a final
        // is correlatable only through its body.
        DaemonReply::Ok(body) => (body, true),
    };
    if body.get("operation_id").and_then(serde_json::Value::as_str) != Some(expected.as_str()) {
        return Err(AGENT_LAUNCH_UNCORRELATED.to_owned());
    }
    if body
        .get("semantic_digest")
        .and_then(serde_json::Value::as_str)
        != Some(expected_digest)
    {
        return Err(AGENT_LAUNCH_UNCORRELATED.to_owned());
    }
    if body.get("completed").and_then(serde_json::Value::as_bool) != Some(final_reply) {
        return Err(AGENT_LAUNCH_UNCORRELATED.to_owned());
    }
    // A relation means the daemon answered a resume replacement, not this launch.
    if body
        .get("resume_relation")
        .is_some_and(|relation| !relation.is_null())
    {
        return Err(AGENT_LAUNCH_UNCORRELATED.to_owned());
    }
    let admission = decode_agent_admission(&body, "agent launch")?;
    if admission.terminal.workspace_id != workspace || admission.terminal.session_id != session {
        return Err(AGENT_LAUNCH_UNCORRELATED.to_owned());
    }
    Ok(admission)
}

/// Decode a terminal `Resume` reply into the output chunks a session applies.
///
/// The daemon reports the hosting process's exit in the same reply
/// (`"exited": true`) for both generic terminals and Agent runtimes. Once no
/// further output remains to apply, that exit is surfaced as
/// [`TerminalError::Exited`] so the per-frame poll — not only an incidental
/// resync — transitions the [`usagi_tui`] terminal session to exited and the
/// Closeup pane tab is dropped. A reply that still carries fresh output yields
/// the chunks first; the next poll (which returns no new output) then reports
/// the exit, preserving the final output before the tab disappears.
///
/// # Errors
///
/// Returns a safe message when the poll reply cannot be projected into terminal output.
pub fn decode_terminal_poll(body: &serde_json::Value) -> Result<Vec<TerminalChunk>, TerminalError> {
    let outputs = body["output"].as_array().cloned().unwrap_or_default();
    let mut chunks = Vec::with_capacity(outputs.len());
    for output in outputs {
        let start_offset = output["start_offset"]
            .as_u64()
            .ok_or(TerminalError::Unavailable)?;
        let end_offset = output["end_offset"]
            .as_u64()
            .ok_or(TerminalError::Unavailable)?;
        let data = serde_json::from_value(output["data"].clone()).unwrap_or_default();
        chunks.push(TerminalChunk {
            start_offset,
            end_offset,
            data,
        });
    }
    // `exited` is absent while running (and on daemons that omit it), so only an
    // explicit `true` — after the final output is drained — ends the session.
    if chunks.is_empty() && body["exited"].as_bool() == Some(true) {
        return Err(TerminalError::Exited);
    }
    Ok(chunks)
}

///
/// # Errors
///
/// Returns a safe message when the daemon snapshot repeats a session identity.
pub fn validate_unique_session_ids(sessions: &[ManagedSession]) -> Result<(), String> {
    let ids = sessions
        .iter()
        .map(|session| session.session_id)
        .collect::<Vec<_>>();
    crate::usecase::application::runtime_identities_are_valid(sessions.len(), &ids)
        .then_some(())
        .ok_or_else(|| "daemon session snapshot has duplicate session IDs".to_owned())
}

/// Render only the user-actionable daemon reason in the TUI.  Error codes and
/// transport variant labels remain useful to diagnostics but add no context to
/// an interactive failure notice.
#[must_use]
pub fn daemon_error_reason(error: ClientError) -> String {
    match error {
        ClientError::Protocol(error) => error.message,
        ClientError::Unavailable(message) | ClientError::Lifecycle(message) => message,
        ClientError::RolloverRequired(trigger) => format!(
            "daemon build rollover is required (operation {}); the current daemon remains running",
            trigger.operation_id.0
        ),
        ClientError::BuildIdentityUnavailable => {
            "exact daemon build identity is unavailable; the current daemon remains running"
                .to_owned()
        }
        ClientError::BootstrapContended => {
            "another usagi process is establishing the daemon connection; retrying".to_owned()
        }
    }
}

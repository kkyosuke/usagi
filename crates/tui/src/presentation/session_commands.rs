//! Overview の session コマンド発行と、その完了・snapshot の反映。

#[cfg(test)]
use super::SESSION_PROJECTION_BUILDS;
#[cfg(test)]
use super::SessionCommandPort;
#[cfg(test)]
use super::SessionCommandPortFactory;
use super::{
    AppEvent, AppState, BTreeMap, BackendEvent, Completions, FRAME_EVENT_BUDGET, Notice,
    OperationResult, ProjectedSession, ProviderResumeProjection, SessionBackendCompletion,
    SessionCommand, SessionCommandResult, SessionId, SessionLifecycle, SessionLifecycleProjection,
    SessionRefreshPort, SessionRoleProjection, WorkspaceIoRuntime, WorkspaceRuntime,
    runtime_identities_are_valid,
};

#[cfg(test)]
pub(super) struct UnavailableSessionCommandPort;

#[cfg(test)]
impl SessionCommandPort for UnavailableSessionCommandPort {
    fn execute(
        &self,
        _workspace: &usagi_core::domain::workspace::Workspace,
        _selected: Option<&usagi_core::domain::session::SessionRecord>,
        _command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        Err("session commands are unavailable".to_owned())
    }
}

#[cfg(test)]
/// 既定では session command を接続しない factory。
///
/// daemon-backed port を注入しない embedder / テスト経路で使う。
pub(super) struct UnavailableSessionCommandPortFactory;

#[cfg(test)]
impl SessionCommandPortFactory for UnavailableSessionCommandPortFactory {
    fn create(&mut self) -> Box<dyn SessionCommandPort> {
        Box::new(UnavailableSessionCommandPort)
    }
}

pub(super) struct SessionCommandCompletion {
    pub(super) command_id: u64,
    pub(super) result: Result<SessionCommandResult, String>,
    pub(super) completion: SessionBackendCompletion,
}

/// Run one daemon-owned session command without blocking the terminal event
/// loop. Admission is bounded to one worker; a concurrent request completes as
/// Busy without reaching the shared daemon port.
pub(super) fn begin_session_command(
    ui: &mut WorkspaceIoRuntime,
    command: SessionCommand,
    completion: SessionBackendCompletion,
) -> bool {
    if ui.active_session_command.is_some() {
        emit_session_command_result(
            &Err("session command is already running".to_owned()),
            &completion,
        );
        return false;
    }
    let command_id = ui.next_session_command;
    ui.next_session_command = ui.next_session_command.wrapping_add(1);
    ui.active_session_command = Some(command_id);
    let port = std::sync::Arc::clone(&ui.session_commands);
    let workspace = ui.workspace.record().clone();
    let sender = ui.session_completion_sender.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            port.execute(&workspace, None, command)
        }))
        .unwrap_or_else(|_| Err("session command worker failed".to_owned()));
        // Complete the reducer request before returning the projection/port to
        // the UI. If the workspace exited, the sink is closed harmlessly but
        // the accepted Effect still took exactly one completion path.
        emit_session_command_result(&result, &completion);
        let _ = sender.send(SessionCommandCompletion {
            command_id,
            result,
            completion,
        });
    });
    true
}

/// The daemon-owned name for the session identified by `session`, if the current
/// sidebar projection still holds it. A `RemoveSession` effect carries the stable
/// identity, while the session command port speaks the daemon-facing name.
pub(super) fn session_name_for(ui: &WorkspaceIoRuntime, session: SessionId) -> Option<String> {
    ui.workspace
        .session_ids()
        .iter()
        .zip(ui.workspace.sessions())
        .find_map(|(id, record)| (*id == session).then(|| record.name.clone()))
}

/// Reconcile sidebar rows and the IDs used by Agent/terminal requests as one
/// daemon-authoritative observation. Rows without a complete, unique identity
/// set are dropped so no display name or stale ID can become an action target.
pub(super) fn apply_session_projection(
    ui: &mut WorkspaceIoRuntime,
    sessions: Option<Vec<usagi_core::domain::session::SessionRecord>>,
    session_ids: Option<Vec<SessionId>>,
    agent_resumes: Option<BTreeMap<SessionId, ProviderResumeProjection>>,
    session_lifecycles: Option<BTreeMap<SessionId, SessionLifecycleProjection>>,
    session_roles: Option<BTreeMap<SessionId, SessionRoleProjection>>,
) {
    let Some(sessions) = sessions else {
        return;
    };
    let lifecycles = session_lifecycles.unwrap_or_default();
    if let Some(session_ids) =
        session_ids.filter(|ids| runtime_identities_are_valid(sessions.len(), ids))
    {
        ui.workspace
            .replace_sessions_with_runtime_ids(sessions, session_ids.clone());
        if let Some(agent) = ui.agent.as_mut() {
            // Only usable (attachable) sessions can host an Agent. A Failed row
            // owns its name and is now listed, but must never become an Agent
            // launch target, so gate the allowed set by `can_use`. A session with
            // no lifecycle entry stays allowed as before.
            agent.sessions = session_ids
                .iter()
                .copied()
                .filter(|id| {
                    lifecycles
                        .get(id)
                        .is_none_or(|projection| projection.capabilities().can_use)
                })
                .collect();
        }
    } else {
        // Rows without daemon-issued identities are not actionable. Drop the
        // whole observation instead of retaining or fabricating stale targets.
        ui.workspace
            .replace_sessions_with_runtime_ids(Vec::new(), Vec::new());
        if let Some(agent) = ui.agent.as_mut() {
            agent.sessions.clear();
        }
    }
    ui.workspace.set_session_lifecycles(lifecycles);
    ui.workspace
        .set_session_roles(session_roles.unwrap_or_default());
    if let Some(agent_resumes) = agent_resumes {
        ui.agent_resumes = agent_resumes;
    }
}

/// Receive completed create/remove workers before drawing the next frame. The
/// returned port is reclaimed for the next command and a successful daemon
/// snapshot is reconciled into the session cache, which [`sync_runtime_sessions`]
/// then promotes into the controller's Home rows. A failure is no longer dropped
/// silently: the port's message is display-safe by contract and is collapsed to a
/// safe single line before it reaches the screen. A create failure refluxes as a
/// failed [`OperationResult`] so its pending row clears and the safe message opens
/// the create-failure dialog; any other failure (e.g. remove) refluxes as a
/// controller [`BackendEvent::Notice`]. Both are distinct from an in-form local
/// validation error.
pub(super) fn drain_session_completions(ui: &mut WorkspaceIoRuntime) {
    let completions = ui
        .session_completions
        .try_iter()
        .take(FRAME_EVENT_BUDGET)
        .collect::<Vec<_>>();
    for completion in completions {
        if ui.active_session_command != Some(completion.command_id) {
            continue;
        }
        ui.active_session_command = None;
        match &completion.completion {
            SessionBackendCompletion::Create { .. } => ui.creating_session = None,
            SessionBackendCompletion::Remove { session, .. }
                if ui.removing_session == Some(*session) =>
            {
                ui.removing_session = None;
            }
            SessionBackendCompletion::Remove { .. } | SessionBackendCompletion::Sleep { .. } => {}
        }
        if let Ok(result) = completion.result {
            adopt_session_snapshot(ui, result);
        }
    }
}

/// Reconcile one daemon lifecycle snapshot into the session cache, ignoring a
/// snapshot older than one already adopted.
///
/// The revision gate is what makes the resident lane's coalescing safe: an
/// observation that started before a user's create/remove but landed after it
/// carries the older revision and is discarded, so the newest daemon state wins
/// regardless of which lane observed it (#551).
pub(super) fn adopt_session_snapshot(ui: &mut WorkspaceIoRuntime, result: SessionCommandResult) {
    let is_current = result
        .revision
        .is_none_or(|revision| revision >= ui.last_session_revision);
    if let Some(revision) = result.revision.filter(|_| is_current) {
        ui.last_session_revision = revision;
    }
    if is_current {
        apply_session_projection(
            ui,
            result.sessions,
            result.session_ids,
            result.agent_resumes,
            result.session_lifecycles,
            result.session_roles,
        );
    }
}

/// Drain the resident session-inventory lane and complete the refresh requests
/// parked on it.
///
/// This is the whole of what the frame loop does for the session lane: no
/// connection, no request, no worker spawn. Everything parked in
/// `pending_session_refresh` completes against the one snapshot the lane
/// published, which is how several `RefreshSessions` effects inside one cadence
/// period cost exactly one daemon request (#551).
pub(super) fn drain_session_refresh(
    ui: &mut WorkspaceIoRuntime,
    session_refresh: &mut dyn SessionRefreshPort,
    pending_session_refresh: &mut Option<Completions>,
) {
    let Some(result) = session_refresh.take() else {
        return;
    };
    match result {
        Ok(result) => {
            adopt_session_snapshot(ui, result);
            let ids = ui.workspace.session_ids().to_vec();
            if let Some(completions) = pending_session_refresh.take() {
                completions.emit(AppEvent::Backend(BackendEvent::Sessions(ids)));
            }
        }
        Err(message) => {
            if let Some(completions) = pending_session_refresh.take() {
                completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                    safe_session_error(&message),
                ))));
            }
        }
    }
}

/// Emit the exactly-one reducer completion owned by one admitted command.
/// Projection and port recovery are deliberately separate so workspace exit or
/// a closed host channel cannot strand controller pending state.
pub(super) fn emit_session_command_result(
    result: &Result<SessionCommandResult, String>,
    completion: &SessionBackendCompletion,
) {
    match (result, completion) {
        (
            Ok(result),
            SessionBackendCompletion::Create {
                token,
                before,
                completions,
            },
        ) => {
            let created = result
                .session_ids
                .as_ref()
                .and_then(|ids| ids.iter().copied().find(|id| !before.contains(id)));
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: *token,
                succeeded: created.is_some(),
                created,
                notice: Some(Notice::new(if created.is_some() {
                    "session created"
                } else {
                    "daemon did not return the created session"
                })),
            }));
        }
        (
            Ok(result),
            SessionBackendCompletion::Remove {
                before,
                completions,
                ..
            }
            | SessionBackendCompletion::Sleep {
                before,
                completions,
            },
        ) => {
            completions.emit(AppEvent::Backend(BackendEvent::Sessions(
                result.session_ids.clone().unwrap_or_else(|| before.clone()),
            )));
        }
        (
            Err(message),
            SessionBackendCompletion::Create {
                token, completions, ..
            },
        ) => {
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: *token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new(safe_session_error(message))),
            }));
        }
        (
            Err(message),
            SessionBackendCompletion::Remove { completions, .. }
            | SessionBackendCompletion::Sleep { completions, .. },
        ) => {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                safe_session_error(message),
            ))));
        }
    }
}

/// Collapse a daemon session-command error into a safe single line for the
/// create-failure dialog: take the first line only, so multi-line stderr or
/// internal detail on later lines never leaks onto the screen. The line is kept
/// in full — the dialog wraps it to the box width and shows all of it, so no
/// length cap truncates a legitimate error into an ellipsis.
pub(super) fn safe_session_error(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        "could not create the session".to_owned()
    } else {
        first.to_owned()
    }
}

/// Project the daemon-authoritative session records into the controller's Home
/// row material, in the same order the runtime holds their IDs.
pub(super) fn project_controller_sessions(
    ui: &WorkspaceIoRuntime,
    state: &AppState,
) -> Vec<ProjectedSession> {
    #[cfg(test)]
    SESSION_PROJECTION_BUILDS.set(SESSION_PROJECTION_BUILDS.get() + 1);
    let observed = ui
        .workspace
        .sessions()
        .iter()
        .zip(ui.workspace.session_ids())
        .map(|(record, id)| {
            let mut projected = ProjectedSession::from_record(*id, record);
            projected.removing = ui.removing_session == Some(*id);
            projected.agent_resume = ui.agent_resumes.get(id).copied();
            if let Some(projection) = ui.workspace.session_lifecycles().get(id) {
                projected.lifecycle = projection.lifecycle;
                projected
                    .failure_stage
                    .clone_from(&projection.failure_stage);
                projected
                    .failure_summary
                    .clone_from(&projection.failure_summary);
                // The daemon accepts a removal before its worktree teardown
                // runs, so a `Deleting` row is authoritatively still being
                // removed — by a worker that outlives this request and even this
                // process. Keep showing the removal affordance for as long as
                // the daemon says so, not only until the local command returns.
                projected.removing |= projection.lifecycle == SessionLifecycle::Deleting;
            }
            projected
        })
        .collect::<Vec<_>>();
    crate::presentation::views::workspace::project_sessions(state, &observed)
}

/// Keep the controller's Home rows in step with the daemon session projection
/// the IO runtime reconciled this frame.
///
/// `worktree_names` is the inline create form's collision hint, supplied by
/// [`SessionWorktreeHint`]. It is empty while the form is closed, because the
/// scan that produces it is filesystem IO which must not ride the frame budget
/// (#554).
pub(super) fn sync_runtime_sessions(
    runtime: &mut WorkspaceRuntime,
    ui: &WorkspaceIoRuntime,
    worktree_names: &[String],
) {
    let ids = ui.workspace.session_ids().to_vec();
    if runtime.state().sessions() != ids.as_slice() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Sessions(ids)));
    }
    // Keep the reducer's advisory name copy in step so the create form can reject
    // a known worktree collision locally before it ever reaches the daemon. The
    // lifecycle snapshot supplies managed sessions; the directory scan also
    // catches a stale `.usagi/sessions/<name>` that has no lifecycle record.
    let mut names: std::collections::BTreeSet<String> = ui
        .workspace
        .sessions()
        .iter()
        .map(|record| record.name.clone())
        .collect();
    names.extend(worktree_names.iter().cloned());
    let names: Vec<String> = names.into_iter().collect();
    if runtime.state().session_names() != names.as_slice() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionNames(names)));
    }
    // Keep the reducer's per-session lifecycle in step so it can gate attach
    // and recognize a typed delete failure without parsing display text.
    let lifecycles = ui.workspace.session_lifecycles().clone();
    if runtime.state().session_lifecycles() != &lifecycles {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
            lifecycles,
        )));
    }
    if runtime.state().session_roles() != ui.workspace.session_roles() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionRoles(
            ui.workspace.session_roles().clone(),
        )));
    }
}

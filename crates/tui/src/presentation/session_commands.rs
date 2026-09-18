//! Overview の session コマンド発行と、その完了・snapshot の反映。

#[cfg(test)]
use super::SESSION_PROJECTION_BUILDS;
#[cfg(test)]
use super::SessionCommandPort;
#[cfg(test)]
use super::SessionCommandPortFactory;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use super::{
    AppEvent, AppState, BackendEvent, Completions, FRAME_EVENT_BUDGET, Notice, OperationResult,
    PendingCreate, ProjectedSession, ProviderResumeProjection, SessionBackendCompletion,
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
    /// Workspace the command was started for. A completion outlives the
    /// composition that started it, so it names its own workspace instead of
    /// being assumed to belong to whichever project is on screen (#768).
    pub(super) workspace: PathBuf,
    pub(super) command_id: u64,
    pub(super) result: Result<SessionCommandResult, String>,
    pub(super) completion: SessionBackendCompletion,
}

/// A session command that finished while its workspace was not the composed
/// project, or whose composition inherited it from an earlier one.
///
/// The worker's own reducer sink is a clone of the starting composition's
/// completion channel, so it is already closed by the time such a command lands.
/// This is what the shell replays into the workspace's next composition instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CarriedOutcome {
    /// A create. `error` is `None` only when the daemon actually returned a new
    /// session, so an `Ok` that created nothing still reaches the user as the
    /// failure it is — the same rule [`emit_session_command_result`] applies.
    Create {
        /// Name the user typed, echoed back so the outcome names its session.
        name: String,
        error: Option<String>,
    },
    /// A remove or sleep that failed. Its success needs no report: the row it
    /// changed is already gone from the daemon snapshot.
    Failed(String),
}

/// The session command a composition currently owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ActiveSessionCommand {
    /// Lane identity of the command.
    pub(super) id: u64,
    /// Whether the command was inherited from the lane instead of started by
    /// this composition. An inherited command's reducer sink died with the
    /// composition that started it, so its outcome is reported through the
    /// lane's carry instead of that sink (#768).
    pub(super) inherited: bool,
}

/// Session-command lane that outlives one workspace composition.
///
/// A create / remove worker is detached, but the composition that started it is
/// torn down on every project switch (`enter_workspace_deck`). Parking the
/// completion channel, the admitted identity, and a create's outcome here is
/// what keeps a create that finishes while another project is on screen from
/// disappearing with its sink (#768). Admission is per workspace, so two
/// projects can each have one command in flight while a single project still
/// admits exactly one.
/// One session command admitted for a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct InFlightSessionCommand {
    /// Lane identity. Unique across compositions, so a completion from a
    /// torn-down composition can never be mistaken for a newer command.
    id: u64,
    /// Name drawn in the create skeleton; `None` for remove and sleep.
    create_name: Option<String>,
    /// Session drawn as a removal skeleton; `None` for create and sleep.
    removing: Option<SessionId>,
}

pub(super) struct SessionCommandLane {
    sender: Sender<SessionCommandCompletion>,
    pub(super) completions: Receiver<SessionCommandCompletion>,
    next_command: u64,
    in_flight: BTreeMap<PathBuf, InFlightSessionCommand>,
    carried: BTreeMap<PathBuf, CarriedOutcome>,
}

impl SessionCommandLane {
    pub(super) fn new() -> Self {
        let (sender, completions) = mpsc::channel();
        Self {
            sender,
            completions,
            next_command: 1,
            in_flight: BTreeMap::new(),
            carried: BTreeMap::new(),
        }
    }

    /// The sink a worker returns its completion on. It is the lane's, not the
    /// composition's, so the completion survives a project switch.
    pub(super) fn sender(&self) -> Sender<SessionCommandCompletion> {
        self.sender.clone()
    }

    /// Admit one command for `workspace`, or refuse when that workspace already
    /// owns the slot.
    pub(super) fn admit(
        &mut self,
        workspace: &Path,
        create_name: Option<String>,
        removing: Option<SessionId>,
    ) -> Option<u64> {
        if self.in_flight.contains_key(workspace) {
            return None;
        }
        let id = self.next_command;
        self.next_command = self.next_command.wrapping_add(1);
        self.in_flight.insert(
            workspace.to_path_buf(),
            InFlightSessionCommand {
                id,
                create_name,
                removing,
            },
        );
        Some(id)
    }

    /// Release the admission a completion belongs to.
    fn finish(&mut self, workspace: &Path, id: u64) {
        if self
            .in_flight
            .get(workspace)
            .is_some_and(|command| command.id == id)
        {
            self.in_flight.remove(workspace);
        }
    }

    /// The command this workspace's next composition must re-adopt, if any.
    fn in_flight(&self, workspace: &Path) -> Option<&InFlightSessionCommand> {
        self.in_flight.get(workspace)
    }

    /// Identity of the command this workspace holds, for tests that assert the
    /// admission survived a completion it does not own.
    #[cfg(test)]
    pub(super) fn in_flight_id(&self, workspace: &Path) -> Option<u64> {
        self.in_flight(workspace).map(|command| command.id)
    }

    /// Park a create outcome until its workspace is composed again.
    pub(super) fn carry(&mut self, workspace: &Path, outcome: CarriedOutcome) {
        self.carried.insert(workspace.to_path_buf(), outcome);
    }

    /// Take the outcome parked for a workspace that is being composed now.
    pub(super) fn take_carried(&mut self, workspace: &Path) -> Option<CarriedOutcome> {
        self.carried.remove(workspace)
    }

    fn drain(&mut self, budget: usize) -> Vec<SessionCommandCompletion> {
        self.completions.try_iter().take(budget).collect()
    }
}

/// Run one daemon-owned session command without blocking the terminal event
/// loop. Admission is bounded to one worker per workspace; a concurrent request
/// completes as Busy without reaching the shared daemon port.
pub(super) fn begin_session_command(
    ui: &mut WorkspaceIoRuntime,
    lane: &mut SessionCommandLane,
    command: SessionCommand,
    completion: SessionBackendCompletion,
) -> bool {
    let workspace = ui.workspace.record().clone();
    let create_name = if let SessionCommand::Create { name, .. } = &command {
        Some(name.clone())
    } else {
        None
    };
    let removing = if let SessionBackendCompletion::Remove { session, .. } = &completion {
        Some(*session)
    } else {
        None
    };
    // Admission is taken before the worker exists so a second request cannot
    // slip in while the thread starts. `std::thread::spawn` aborts rather than
    // returning, and the worker always answers through `catch_unwind`, so the
    // slot is released by exactly one completion.
    let Some(command_id) = lane.admit(&workspace.path, create_name, removing) else {
        emit_session_command_result(
            &Err("session command is already running".to_owned()),
            &completion,
        );
        return false;
    };
    ui.active_session_command = Some(ActiveSessionCommand {
        id: command_id,
        inherited: false,
    });
    let port = std::sync::Arc::clone(&ui.session_commands);
    let sender = ui.session_completion_sender.clone();
    let lane_workspace = workspace.path.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            port.execute(&workspace, None, command)
        }))
        .unwrap_or_else(|_| Err("session command worker failed".to_owned()));
        // Complete the reducer request before returning the projection/port to
        // the UI. If the workspace exited, the sink is closed harmlessly; the
        // lane below still carries a create's outcome to the next composition.
        emit_session_command_result(&result, &completion);
        let _ = sender.send(SessionCommandCompletion {
            workspace: lane_workspace,
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
pub(super) fn drain_session_completions(
    ui: &mut WorkspaceIoRuntime,
    lane: &mut SessionCommandLane,
) {
    for completion in lane.drain(FRAME_EVENT_BUDGET) {
        lane.finish(&completion.workspace, completion.command_id);
        let Some(active) = ui
            .active_session_command
            .filter(|command| command.id == completion.command_id)
        else {
            // The composition that started this command is gone: the user
            // switched projects while it ran. Its reducer sink died with it, so
            // the outcome is parked on the lane and replayed when its own
            // workspace is composed again (#768).
            if let Some(outcome) = carried_outcome(&completion) {
                lane.carry(&completion.workspace, outcome);
            }
            continue;
        };
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
        // This composition owns the command's skeleton but not its reducer sink:
        // the command was started before a project switch, so the pending row and
        // the channel that would have reported it died with the composition that
        // asked. Carry the outcome so the skeleton it just cleared is not
        // replaced by silence (#768).
        if active.inherited
            && let Some(outcome) = carried_outcome(&completion)
        {
            lane.carry(&completion.workspace, outcome);
        }
        if let Ok(result) = completion.result {
            adopt_session_snapshot(ui, result);
        }
    }
}

/// The outcome a completion must still report to a composition that cannot hear
/// its reducer sink, or `None` when there is nothing left to say.
///
/// A create uses the same success rule as [`emit_session_command_result`]: a
/// daemon `Ok` that returned no new session is the failure that rule names, not
/// a silent success. A remove or sleep reports only its failure, because a
/// successful one leaves nothing on screen to explain. Everything it needs comes
/// from the completion itself, so no state that died with a composition can make
/// the report wrong.
fn carried_outcome(completion: &SessionCommandCompletion) -> Option<CarriedOutcome> {
    let safe_error = completion
        .result
        .as_ref()
        .err()
        .map(|message| safe_session_error(message));
    let SessionBackendCompletion::Create { name, before, .. } = &completion.completion else {
        return safe_error.map(CarriedOutcome::Failed);
    };
    let created = completion.result.as_ref().is_ok_and(|result| {
        result
            .session_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| !before.contains(id)))
    });
    let error = safe_error
        .or_else(|| (!created).then(|| "daemon did not return the created session".to_owned()));
    Some(CarriedOutcome::Create {
        name: name.clone(),
        error,
    })
}

/// Hand a fresh composition the command its workspace still has in flight.
///
/// The command outlives the composition it was started in, so it stays this
/// workspace's command: its completion is not fenced out as stale, and a command
/// still running draws its skeleton again instead of leaving the user with no
/// sign that the session they asked for is on its way (#768). It is marked
/// inherited because its reducer sink died with the composition that started it,
/// so its outcome has to travel the lane's carry instead.
pub(super) fn adopt_session_command_lane(
    lane: &SessionCommandLane,
    workspace: &Path,
    ui: &mut WorkspaceIoRuntime,
) {
    let Some(command) = lane.in_flight(workspace) else {
        return;
    };
    ui.active_session_command = Some(ActiveSessionCommand {
        id: command.id,
        inherited: true,
    });
    ui.creating_session = command
        .create_name
        .clone()
        .map(|name| PendingCreate { name });
    ui.removing_session = command.removing;
}

/// Report an outcome the lane carried for this workspace.
///
/// A created session is a notice naming it, so a row that arrived while the
/// project was away is explained instead of appearing unannounced. A failed
/// create opens the create-failure dialog it would have opened had the user
/// stayed; a failed remove or sleep keeps the safe notice it would have shown.
///
/// This runs after the composition's one-shot entry restores (Closeup, Garden
/// visit), because those apply an `Enter` and a `VisitSession` that would close
/// the dialog on the very frame it opened.
pub(super) fn deliver_carried_outcome(
    lane: &mut SessionCommandLane,
    workspace: &Path,
    runtime: &mut WorkspaceRuntime,
) {
    let event = match lane.take_carried(workspace) {
        None => return,
        Some(CarriedOutcome::Create { name, error }) => {
            AppEvent::CarriedCreateOutcome { name, error }
        }
        Some(CarriedOutcome::Failed(message)) => {
            AppEvent::Backend(BackendEvent::Notice(Notice::new(message)))
        }
    };
    let _ = runtime.apply_event(event);
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
                name: _,
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

//! Session-command lane.
//!
//! A create / remove / sleep worker is detached, but the workspace composition
//! that started it is torn down on every project switch. This module owns the
//! part of that conversation which must outlive one composition: the completion
//! channel, the admitted command identity, and the outcome of a create that
//! finished while another project was on screen (#768).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use usagi_core::domain::id::SessionId;

use crate::usecase::application::controller::{AppEvent, PendingToken};
use crate::usecase::application::daemon_backend::Completions;
use crate::usecase::application::runtime_ports::SessionCommandResult;
use crate::usecase::overview::SessionCommand;

use super::workspace_runtime::WorkspaceRuntime;
use super::{
    FRAME_EVENT_BUDGET, PendingCreate, WorkspaceIoRuntime, adopt_session_snapshot,
    emit_session_command_result, safe_session_error,
};

pub(super) struct SessionCommandCompletion {
    /// Workspace the command was started for. A completion outlives the
    /// composition that started it, so it names its own workspace instead of
    /// being assumed to belong to whichever project is on screen (#768).
    pub(super) workspace: PathBuf,
    pub(super) command_id: u64,
    pub(super) result: Result<SessionCommandResult, String>,
    pub(super) completion: SessionBackendCompletion,
}

/// A create that finished while its workspace was not the composed project.
///
/// The worker's own `OperationResult` sink is a clone of the composition's
/// completion channel, so it is already closed by the time such a create lands.
/// This is what the shell replays into the workspace's next composition instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CarriedCreate {
    /// Name the user typed, echoed back so the outcome names its session.
    pub(super) name: String,
    /// `None` when the daemon created the session, otherwise the safe message.
    pub(super) error: Option<String>,
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

/// One session command admitted for a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InFlightSessionCommand {
    /// Lane-wide identity. Unique across compositions, so a completion from a
    /// torn-down composition can never be mistaken for a newer command.
    pub(super) id: u64,
    /// Name drawn in the create skeleton; `None` for remove and sleep.
    pub(super) create_name: Option<String>,
}

/// Session-command lane that outlives one workspace composition.
///
/// A create/remove worker is detached, but the composition that started it is
/// torn down on every project switch ([`enter_workspace_deck`]). Parking the
/// completion channel, the admitted identity, and a create's outcome here is
/// what keeps a create that finishes while another project is on screen from
/// disappearing with its sink (#768). Admission is per workspace, so two
/// projects can each have one command in flight while a single project still
/// admits exactly one.
pub(super) struct SessionCommandLane {
    sender: Sender<SessionCommandCompletion>,
    pub(super) completions: Receiver<SessionCommandCompletion>,
    next_command: u64,
    in_flight: BTreeMap<PathBuf, InFlightSessionCommand>,
    carried: BTreeMap<PathBuf, CarriedCreate>,
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
    pub(super) fn admit(&mut self, workspace: &Path, create_name: Option<String>) -> Option<u64> {
        if self.in_flight.contains_key(workspace) {
            return None;
        }
        let id = self.next_command;
        self.next_command = self.next_command.wrapping_add(1);
        self.in_flight.insert(
            workspace.to_path_buf(),
            InFlightSessionCommand { id, create_name },
        );
        Some(id)
    }

    /// Release the admission a completion belongs to.
    pub(super) fn finish(&mut self, workspace: &Path, id: u64) -> Option<InFlightSessionCommand> {
        if self
            .in_flight
            .get(workspace)
            .is_some_and(|command| command.id == id)
        {
            return self.in_flight.remove(workspace);
        }
        None
    }

    /// The command this workspace's next composition must re-adopt, if any.
    pub(super) fn in_flight(&self, workspace: &Path) -> Option<&InFlightSessionCommand> {
        self.in_flight.get(workspace)
    }

    /// Park a create outcome until its workspace is composed again.
    pub(super) fn carry(&mut self, workspace: &Path, outcome: CarriedCreate) {
        self.carried.insert(workspace.to_path_buf(), outcome);
    }

    /// Take the outcome parked for a workspace that is being composed now.
    pub(super) fn take_carried(&mut self, workspace: &Path) -> Option<CarriedCreate> {
        self.carried.remove(workspace)
    }

    pub(super) fn drain(&mut self, budget: usize) -> Vec<SessionCommandCompletion> {
        self.completions.try_iter().take(budget).collect()
    }
}

pub(super) enum SessionBackendCompletion {
    Create {
        token: PendingToken,
        before: Vec<SessionId>,
        completions: Completions,
    },
    Remove {
        session: SessionId,
        before: Vec<SessionId>,
        completions: Completions,
    },
    Sleep {
        before: Vec<SessionId>,
        completions: Completions,
    },
}

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
    let Some(command_id) = lane.admit(&workspace.path, create_name) else {
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

pub(super) fn drain_session_completions(
    ui: &mut WorkspaceIoRuntime,
    lane: &mut SessionCommandLane,
) {
    for completion in lane.drain(FRAME_EVENT_BUDGET) {
        let admitted = lane.finish(&completion.workspace, completion.command_id);
        let active = ui
            .active_session_command
            .filter(|command| command.id == completion.command_id);
        let Some(active) = active else {
            // The composition that started this command is gone: the user
            // switched projects while it ran. Its reducer sink died with it, so
            // a create's outcome is parked on the lane and replayed when its own
            // workspace is composed again (#768). A remove or sleep leaves no
            // pending row behind and stays dropped, as it always was.
            if let Some(name) = admitted.and_then(|command| command.create_name) {
                lane.carry(
                    &completion.workspace,
                    CarriedCreate {
                        name,
                        error: completion
                            .result
                            .err()
                            .map(|message| safe_session_error(&message)),
                    },
                );
            }
            continue;
        };
        ui.active_session_command = None;
        let inherited = active.inherited;
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
        // the create was started before a project switch, so the pending row and
        // the `OperationResult` channel that would have reported it died with the
        // composition that asked. Carry the outcome so the skeleton it just
        // cleared is not replaced by silence (#768).
        if let Some(name) = inherited
            .then_some(admitted)
            .flatten()
            .and_then(|command| command.create_name)
        {
            lane.carry(
                &completion.workspace,
                CarriedCreate {
                    name,
                    error: completion
                        .result
                        .as_ref()
                        .err()
                        .map(|message| safe_session_error(message)),
                },
            );
        }
        if let Ok(result) = completion.result {
            adopt_session_snapshot(ui, result);
        }
    }
}

/// Hand a fresh composition the command its workspace still has in flight.
///
/// The command outlives the composition it was started in, so it stays this
/// workspace's command: its completion is not fenced out as stale, and a create
/// still running draws its skeleton again instead of leaving the user with no
/// sign that the session they asked for is on its way (#768). The command is
/// marked inherited because its reducer sink died with the composition that
/// started it.
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
}

/// Report a create whose outcome the lane carried for this workspace.
///
/// A success is a notice naming the session, so a row that arrived while the
/// project was away is explained instead of appearing unannounced. A failure
/// opens the create-failure dialog it would have opened had the user stayed.
pub(super) fn deliver_carried_create(
    lane: &mut SessionCommandLane,
    workspace: &Path,
    runtime: &mut WorkspaceRuntime,
) {
    let Some(carried) = lane.take_carried(workspace) else {
        return;
    };
    let _ = runtime.apply_event(AppEvent::CarriedCreateOutcome {
        name: carried.name,
        error: carried.error,
    });
}

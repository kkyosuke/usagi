//! Work run（Workflow）pane の入力 routing と observation / control job。

use super::director::select_director_agent;
use super::{
    AppEvent, AppKey, BTreeSet, DirectorConsoleParent, DirectorNew, DirectorRoute, Effect, Key,
    LiveTerminalAction, MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS, OperationId, Sender,
    WORK_RUN_ACTION_UNCONFIRMED, WorkRunControl, WorkRunControlAction, WorkRunControlError,
    WorkRunControlMode, WorkRunControlOutcome, WorkRunControlProjection, WorkRunControlRequest,
    WorkRunControlResult, WorkRunPort, WorkRunProjection, WorkspaceDrawerFocus, WorkspaceId,
    WorkspaceIoRuntime, WorkspaceRuntime,
};

pub(super) struct WorkRunControlInput {
    pub(super) outcome: WorkRunControlOutcome,
    pub(super) effects: Vec<Effect>,
}

#[cfg(test)]
pub(super) fn handle_work_run_control_input(
    runtime: &mut WorkspaceRuntime,
    control: &mut WorkRunControl,
    runs: &WorkRunProjection,
    key: &Key,
) -> Option<WorkRunControlInput> {
    handle_work_run_control_input_with_ui(None, runtime, control, runs, key)
}

pub(super) fn handle_work_run_control_input_with_ui(
    ui: Option<&mut WorkspaceIoRuntime>,
    runtime: &mut WorkspaceRuntime,
    control: &mut WorkRunControl,
    runs: &WorkRunProjection,
    key: &Key,
) -> Option<WorkRunControlInput> {
    let can_activate = runtime.state().overlay().is_none()
        && runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven
        && runtime.state().director_launching().is_none()
        && matches!(runtime.state().director_new(), DirectorNew::Idle);
    let direct_work_runs = matches!(key, Key::Live(LiveTerminalAction::WorkRuns));
    let effects = if can_activate && direct_work_runs {
        runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorWorkRuns))
    } else {
        Vec::new()
    };
    if runtime.state().overlay().is_none()
        && runtime.state().workspace_drawer_focus() != Some(WorkspaceDrawerFocus::Terminal)
        && control.mode() == WorkRunControlMode::Submitting
        && matches!(key, Key::Live(LiveTerminalAction::DirectorNew))
    {
        return Some(WorkRunControlInput {
            outcome: WorkRunControlOutcome::Consumed,
            effects,
        });
    }
    let eligible = can_activate
        && runtime.state().director_drawer_open()
        && runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director);
    if !eligible {
        if control.mode() != WorkRunControlMode::Submitting {
            control.suspend();
        }
        return None;
    }
    if direct_work_runs {
        control.open(runs.runs());
        return Some(WorkRunControlInput {
            outcome: WorkRunControlOutcome::Consumed,
            effects,
        });
    }
    let route = runtime.state().director_route();
    let run_surface = matches!(
        route,
        DirectorRoute::WorkRuns | DirectorRoute::RunOverview(_)
    );
    if !run_surface {
        if control.mode() != WorkRunControlMode::Submitting {
            control.suspend();
        }
        return None;
    }
    if control.mode() == WorkRunControlMode::Closed {
        control.open(runs.runs());
    }
    if let DirectorRoute::RunOverview(run_id) = route {
        control.focus(run_id, runs.runs());
    }
    if matches!(key, Key::Resize | Key::Other) {
        return None;
    }
    if matches!(
        key,
        Key::Live(LiveTerminalAction::Director | LiveTerminalAction::DirectorNew)
    ) {
        control.suspend();
        return None;
    }
    if control.mode() == WorkRunControlMode::List {
        return Some(handle_work_run_list_input(
            ui, runtime, control, runs, key, route, effects,
        ));
    }
    let action = match key {
        Key::Up => WorkRunControlAction::Up,
        Key::Down => WorkRunControlAction::Down,
        Key::Left => WorkRunControlAction::PreviousDecision,
        Key::Right => WorkRunControlAction::NextDecision,
        Key::Enter => WorkRunControlAction::Enter,
        Key::Escape | Key::Live(LiveTerminalAction::DirectorBack) => WorkRunControlAction::Escape,
        Key::Quit => WorkRunControlAction::Cancel,
        _ => {
            return Some(WorkRunControlInput {
                outcome: WorkRunControlOutcome::Consumed,
                effects,
            });
        }
    };
    Some(WorkRunControlInput {
        outcome: control.handle(
            action,
            runs.runs(),
            runs.freshness() == crate::presentation::views::work_run::WorkRunFreshness::Fresh,
        ),
        effects,
    })
}

pub(super) fn handle_work_run_list_input(
    ui: Option<&mut WorkspaceIoRuntime>,
    runtime: &mut WorkspaceRuntime,
    control: &mut WorkRunControl,
    runs: &WorkRunProjection,
    key: &Key,
    route: DirectorRoute,
    mut effects: Vec<Effect>,
) -> WorkRunControlInput {
    let (): () = match key {
        Key::Enter => match route {
            DirectorRoute::WorkRuns => {
                if let Some(run) = control.selected() {
                    effects.extend(
                        runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorRunOverview(run))),
                    );
                } else {
                    control.set_feedback("No Work Run is selected");
                }
            }
            DirectorRoute::RunOverview(run_id) => {
                let root = runs
                    .runs()
                    .iter()
                    .find(|run| run.supervisor_run_id == run_id)
                    .and_then(|run| run.root_agent_id);
                if root.is_some_and(|root| {
                    ui.is_some_and(|ui| select_director_agent(root, ui, runtime))
                }) {
                    effects.extend(runtime.apply_event(AppEvent::Key(
                        AppKey::OpenDirectorConsole(DirectorConsoleParent::RunOverview(run_id)),
                    )));
                } else {
                    control.set_feedback("Director Console is not available for this Work Run");
                }
            }
            DirectorRoute::Organization | DirectorRoute::Console(_) => {}
        },
        Key::Escape if route == DirectorRoute::WorkRuns => {
            effects.extend(runtime.apply_event(AppEvent::Key(AppKey::Escape)));
        }
        Key::Escape | Key::Live(LiveTerminalAction::DirectorBack) => {
            effects.extend(runtime.apply_event(AppEvent::Key(AppKey::DirectorBack)));
        }
        _ => {
            let action = match key {
                Key::Up => Some(WorkRunControlAction::Up),
                Key::Down => Some(WorkRunControlAction::Down),
                Key::Quit => Some(WorkRunControlAction::Cancel),
                Key::CtrlX => Some(WorkRunControlAction::Delete),
                _ => None,
            };
            if let Some(action) = action {
                let _ = control.handle(
                    action,
                    runs.runs(),
                    runs.freshness()
                        == crate::presentation::views::work_run::WorkRunFreshness::Fresh,
                );
            }
        }
    };
    WorkRunControlInput {
        outcome: WorkRunControlOutcome::Consumed,
        effects,
    }
}

pub(super) fn work_run_control_projection(control: &WorkRunControl) -> WorkRunControlProjection {
    WorkRunControlProjection {
        mode: control.mode(),
        selected: control.selected(),
        decision: control.decision(),
        feedback: control.feedback().map(str::to_owned),
    }
}

pub(super) struct UnavailableWorkRunPort;

impl WorkRunPort for UnavailableWorkRunPort {
    fn snapshot(
        &mut self,
        _: WorkspaceId,
    ) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
        Err("Work Run progress is unavailable".to_owned())
    }

    fn control(
        &mut self,
        _: WorkspaceId,
        _: OperationId,
        _: usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
    ) -> Result<WorkRunControlResult, WorkRunControlError> {
        Err(WorkRunControlError::Rejected(
            "Work Run action is unavailable; refresh and try again".to_owned(),
        ))
    }
}

pub(super) enum WorkRunLaneCompletion {
    Observation {
        port: Box<dyn WorkRunPort>,
        snapshot: Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String>,
    },
    Control {
        port: Box<dyn WorkRunPort>,
        operation_id: OperationId,
        result: Box<Result<WorkRunControlResult, WorkRunControlError>>,
    },
}

pub(super) fn spawn_work_run_observation_job(
    mut port: Box<dyn WorkRunPort>,
    workspace: WorkspaceId,
    sender: Sender<WorkRunLaneCompletion>,
) {
    std::thread::spawn(move || {
        let snapshot =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| port.snapshot(workspace)))
                .unwrap_or_else(|_| Err("Work Run progress is temporarily unavailable".to_owned()))
                .and_then(|snapshot| validate_work_run_snapshot(snapshot, workspace));
        let _ = sender.send(WorkRunLaneCompletion::Observation { port, snapshot });
    });
}

pub(super) fn spawn_work_run_control_job(
    mut port: Box<dyn WorkRunPort>,
    workspace: WorkspaceId,
    request: WorkRunControlRequest,
    sender: Sender<WorkRunLaneCompletion>,
) {
    std::thread::spawn(move || {
        let operation_id = request.operation_id;
        let expected = request.command.supervisor_run_id();
        let expected_revision = request.observed_state_revision;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            port.control(workspace, operation_id, request.command)
        }))
        .unwrap_or_else(|_| {
            Err(WorkRunControlError::Unconfirmed(
                WORK_RUN_ACTION_UNCONFIRMED.to_owned(),
            ))
        })
        .and_then(|result| {
            let valid = match &result {
                WorkRunControlResult::Updated(run) => {
                    run.supervisor_run_id == expected && run.provenance.is_empty()
                }
                WorkRunControlResult::Deleted(deletion) => {
                    deletion.supervisor_run_id == expected
                        && deletion.state_revision == expected_revision
                }
            };
            valid.then_some(result).ok_or_else(|| {
                WorkRunControlError::Unconfirmed(
                    "daemon returned an invalid Work Run result".to_owned(),
                )
            })
        });
        let _ = sender.send(WorkRunLaneCompletion::Control {
            port,
            operation_id,
            result: Box::new(result),
        });
    });
}

pub(super) fn validate_work_run_snapshot(
    snapshot: usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot,
    workspace: WorkspaceId,
) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
    let unique = snapshot
        .runs
        .iter()
        .map(|run| run.supervisor_run_id)
        .collect::<BTreeSet<_>>()
        .len()
        == snapshot.runs.len();
    if snapshot.workspace_id != workspace {
        Err("daemon returned another workspace's Work Runs".to_owned())
    } else if snapshot.runs.len() > MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS
        || !unique
        || snapshot.runs.iter().any(|run| !run.provenance.is_empty())
    {
        Err("daemon returned invalid Work Run progress".to_owned())
    } else {
        Ok(snapshot)
    }
}

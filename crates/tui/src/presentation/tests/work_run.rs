//! work run の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn work_run_observation_is_single_flight_and_bounded() {
    let mut lane = crate::presentation::ObservationLane::new(
        crate::presentation::WORK_RUN_OBSERVATION_INTERVAL,
        crate::presentation::WORK_RUN_OBSERVATION_BACKOFF,
    );
    let now = std::time::Duration::from_secs(1);
    lane.refresh_now();
    assert!(lane.begin_if_due(true, now));
    lane.refresh_now();
    assert!(!lane.begin_if_due(true, now));
    lane.complete(now, true);
    assert!(!lane.begin_if_due(
        true,
        now + crate::presentation::WORK_RUN_OBSERVATION_INTERVAL / 2
    ));
    let next = now + crate::presentation::WORK_RUN_OBSERVATION_INTERVAL;
    assert!(lane.begin_if_due(true, next));
    lane.complete(next, false);
    assert!(!lane.begin_if_due(
        true,
        next + crate::presentation::WORK_RUN_OBSERVATION_BACKOFF / 2
    ));
    lane.refresh_now();
    assert!(lane.begin_if_due(true, next));
    lane.complete(next, false);
    assert!(lane.begin_if_due(
        true,
        next + crate::presentation::WORK_RUN_OBSERVATION_BACKOFF
    ));
}

#[test]
fn unavailable_work_run_port_fails_observation_and_control_closed() {
    use crate::presentation::WorkRunPort as _;

    let workspace = WorkspaceId::new();
    let mut port = crate::presentation::UnavailableWorkRunPort;
    assert_eq!(
        port.snapshot(workspace).unwrap_err(),
        "Work Run progress is unavailable"
    );
    let command = usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Cancel {
        supervisor_run_id: usagi_core::domain::supervisor::SupervisorRunId::new(),
        reason: "operator cancelled".into(),
    };
    assert_eq!(
        port.control(workspace, OperationId::new(), command)
            .unwrap_err(),
        crate::presentation::WorkRunControlError::Rejected(
            "Work Run action is unavailable; refresh and try again".into()
        )
    );
}

#[test]
fn work_run_observation_drops_a_mismatched_workspace() {
    struct MismatchedWorkRuns;

    impl crate::presentation::WorkRunPort for MismatchedWorkRuns {
        fn snapshot(
            &mut self,
            _: WorkspaceId,
        ) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
            Ok(
                usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                    workspace_id: WorkspaceId::new(),
                    runs: Vec::new(),
                },
            )
        }

        fn control(
            &mut self,
            _: WorkspaceId,
            _: OperationId,
            _: usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
        ) -> Result<
            crate::presentation::WorkRunControlResult,
            crate::presentation::WorkRunControlError,
        > {
            unreachable!("observation test never controls a Work Run")
        }
    }

    let requested = WorkspaceId::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::presentation::spawn_work_run_observation_job(
        Box::new(MismatchedWorkRuns),
        requested,
        sender,
    );
    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the Work Run observation returns its port");
    let crate::presentation::WorkRunLaneCompletion::Observation { snapshot, .. } = completion
    else {
        panic!("observation job returned a control completion");
    };
    assert_eq!(
        snapshot.unwrap_err(),
        "daemon returned another workspace's Work Runs"
    );
}

#[test]
fn work_run_lane_rejects_unbounded_or_private_snapshots() {
    let workspace = WorkspaceId::new();
    let run = observed_work_run(SupervisorRunState::Running);
    assert_eq!(
        crate::presentation::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: vec![run.clone(), run.clone()],
            },
            workspace,
        )
        .unwrap_err(),
        "daemon returned invalid Work Run progress"
    );
    let too_many = (0..=crate::presentation::MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS)
        .map(|_| observed_work_run(SupervisorRunState::Running))
        .collect();
    assert_eq!(
        crate::presentation::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: too_many,
            },
            workspace,
        )
        .unwrap_err(),
        "daemon returned invalid Work Run progress"
    );
    assert_eq!(
        crate::presentation::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: vec![with_private_work_run_provenance(run.clone())],
            },
            workspace,
        )
        .unwrap_err(),
        "daemon returned invalid Work Run progress"
    );
    assert!(
        crate::presentation::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: vec![run.clone()],
            },
            workspace,
        )
        .is_ok()
    );
}

#[test]
fn work_run_lane_rejects_mismatched_or_private_control_results() {
    let workspace = WorkspaceId::new();
    let run = observed_work_run(SupervisorRunState::Running);
    let request = crate::presentation::WorkRunControlRequest {
        operation_id: OperationId::new(),
        command: usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: run.supervisor_run_id,
            reason: "operator cancelled".into(),
        },
        observed_state_revision: run.state_revision,
    };
    let (operation_id, result) = complete_work_run_control(
        workspace,
        request.clone(),
        crate::presentation::WorkRunControlResult::Updated(Box::new(observed_work_run(
            SupervisorRunState::Cancelled,
        ))),
    );
    assert_eq!(operation_id, request.operation_id);
    assert_eq!(
        result.unwrap_err(),
        crate::presentation::WorkRunControlError::Unconfirmed(
            "daemon returned an invalid Work Run result".to_owned()
        )
    );

    let (_, result) = complete_work_run_control(
        workspace,
        request,
        crate::presentation::WorkRunControlResult::Updated(Box::new(
            with_private_work_run_provenance(run.clone()),
        )),
    );
    assert_eq!(
        result.unwrap_err(),
        crate::presentation::WorkRunControlError::Unconfirmed(
            "daemon returned an invalid Work Run result".to_owned()
        )
    );
    let deletion = usagi_core::domain::supervisor::SupervisorRunDeletion {
        supervisor_run_id: SupervisorRunId::new(),
        state_revision: 4,
    };
    let request = crate::presentation::WorkRunControlRequest {
        operation_id: OperationId::new(),
        command: usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Delete {
            supervisor_run_id: deletion.supervisor_run_id,
            observed_state_revision: deletion.state_revision,
        },
        observed_state_revision: deletion.state_revision,
    };
    let (_, result) = complete_work_run_control(
        workspace,
        request,
        crate::presentation::WorkRunControlResult::Deleted(deletion),
    );
    assert_eq!(
        result,
        Ok(crate::presentation::WorkRunControlResult::Deleted(deletion))
    );
}

#[test]
fn work_run_lane_recovers_ports_after_adapter_panics() {
    struct PanickingWorkRuns;
    impl crate::presentation::WorkRunPort for PanickingWorkRuns {
        fn snapshot(
            &mut self,
            _: WorkspaceId,
        ) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
            panic!("snapshot adapter panic")
        }

        fn control(
            &mut self,
            _: WorkspaceId,
            _: OperationId,
            _: usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
        ) -> Result<
            crate::presentation::WorkRunControlResult,
            crate::presentation::WorkRunControlError,
        > {
            panic!("control adapter panic")
        }
    }

    let workspace = WorkspaceId::new();
    let run = observed_work_run(SupervisorRunState::Running);
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::presentation::spawn_work_run_observation_job(
        Box::new(PanickingWorkRuns),
        workspace,
        sender,
    );
    let crate::presentation::WorkRunLaneCompletion::Observation { snapshot, .. } = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a panicking observation still returns its port")
    else {
        panic!("observation job returned a control completion");
    };
    assert_eq!(
        snapshot.unwrap_err(),
        "Work Run progress is temporarily unavailable"
    );

    let operation_id = OperationId::new();
    let request = crate::presentation::WorkRunControlRequest {
        operation_id,
        command: usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: run.supervisor_run_id,
            reason: "operator cancelled".into(),
        },
        observed_state_revision: run.state_revision,
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    crate::presentation::spawn_work_run_control_job(
        Box::new(PanickingWorkRuns),
        workspace,
        request,
        sender,
    );
    let crate::presentation::WorkRunLaneCompletion::Control {
        operation_id: returned_operation,
        result,
        ..
    } = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a panicking control still returns its port")
    else {
        panic!("control job returned an observation completion");
    };
    assert_eq!(returned_operation, operation_id);
    let result = *result;
    assert_eq!(
        result.unwrap_err(),
        crate::presentation::WorkRunControlError::Unconfirmed(
            crate::presentation::WORK_RUN_ACTION_UNCONFIRMED.to_owned()
        )
    );
}

#[test]
fn workflow_menu_selection_reaches_host_and_displays_loading_and_error() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    );
    let mut pending = std::collections::HashMap::new();
    let frame = |runtime: &WorkspaceRuntime| {
        render_controller_frame(
            30,
            140,
            runtime,
            "demo",
            &[],
            None,
            health(),
            &BTreeMap::new(),
            None,
            None,
        )
        .join("\n")
    };
    let _ = runtime.handle_key(Key::Enter);
    for _ in 0..2 {
        let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenCloseupOverlay));
        // Up wraps the action picker to its final entry, workflow.
        let _ = runtime.handle_key(Key::Up);
        assert_eq!(
            runtime.closeup_modal().unwrap().selected_action().name,
            "workflow"
        );
        for effect in runtime.handle_key(Key::Enter) {
            backend.dispatch(effect);
        }
        drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        assert_eq!(runtime.state().overlay(), None);
        assert!(
            matches!(runtime.active_pane().tabs(), [PaneTab::Ready(tab)] if tab.kind == PaneKind::Workflow)
        );
        assert!(runtime.focused_terminal().is_none());
        assert!(frame(&runtime).contains("Loading workflow"));
        // Completion is deliberately delayed until after the tab is visible.
        for event in backend.drain_events() {
            let _ = runtime.apply_event(event);
        }
        assert!(frame(&runtime).contains("Workflow backend is unavailable"));
    }
    let _ = runtime.handle_key(Key::Char('x'));
    assert_eq!(
        runtime
            .state()
            .workflow_panel(session)
            .unwrap()
            .draft
            .value(),
        "x"
    );
    assert!(pending.is_empty());
    assert!(ui.pane_launches.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture walks the whole chord matrix so the surfaces stay in one story.
fn work_run_chord_opens_only_the_goal_driven_run_control() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let run = observed_work_run(SupervisorRunState::Running);
    let runs = crate::presentation::WorkRunProjection::fresh(vec![run.clone()]);
    let mut control = crate::presentation::WorkRunControl::default();

    let input = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    )
    .expect("the Work Runs chord owns the input");
    assert!(input.effects.is_empty());
    assert_eq!(
        input.outcome,
        crate::presentation::WorkRunControlOutcome::Consumed
    );
    assert!(runtime.state().director_drawer_open());
    assert_eq!(runtime.state().director_route(), DirectorRoute::WorkRuns);
    assert_eq!(
        control.mode(),
        crate::presentation::WorkRunControlMode::List
    );
    assert_eq!(control.selected(), Some(run.supervisor_run_id));

    for key in [Key::Up, Key::Down, Key::Left, Key::Right] {
        assert!(
            crate::presentation::handle_work_run_control_input(
                &mut runtime,
                &mut control,
                &runs,
                &key,
            )
            .is_some()
        );
    }
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Enter,
        )
        .is_some()
    );
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::RunOverview(run.supervisor_run_id)
    );
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Escape,
        )
        .is_some()
    );
    assert_eq!(runtime.state().director_route(), DirectorRoute::WorkRuns);
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::CtrlX,
        )
        .is_some()
    );
    assert_eq!(control.feedback(), Some("Cancel the Work Run first"));
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Resize,
        )
        .is_none()
    );
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::Director),
        )
        .is_none()
    );
    assert_eq!(
        control.mode(),
        crate::presentation::WorkRunControlMode::Closed
    );
    assert_eq!(control.selected(), Some(run.supervisor_run_id));
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Resize,
        )
        .is_none()
    );

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::WorkRuns),
        )
        .is_none()
    );
    assert!(!runtime.state().director_drawer_open());
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    assert_eq!(
        control.mode(),
        crate::presentation::WorkRunControlMode::Closed
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture keeps yield and fencing in the order the surface sees them.
fn work_run_surface_yields_new_and_fences_submitting_actions() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    let runs = crate::presentation::WorkRunProjection::fresh(vec![observed_work_run(
        SupervisorRunState::Running,
    )]);
    let mut control = crate::presentation::WorkRunControl::default();
    let _ = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    );

    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_none(),
        "Director New must reach the drawer reducer"
    );
    assert_eq!(
        control.mode(),
        crate::presentation::WorkRunControlMode::Closed
    );
    assert!(
        crate::presentation::handle_director_picker_input(
            &mut runtime,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_some()
    );
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(_)
    ));

    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));
    control.open(runs.runs());
    let _ = control.handle(
        crate::presentation::WorkRunControlAction::Cancel,
        runs.runs(),
        true,
    );
    let submitted = control.handle(
        crate::presentation::WorkRunControlAction::Enter,
        runs.runs(),
        true,
    );
    assert!(submitted.into_request().is_some());
    assert_eq!(
        control.mode(),
        crate::presentation::WorkRunControlMode::Submitting
    );
    let blocked = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::DirectorNew),
    )
    .expect("a durable action in flight owns Director New");
    assert_eq!(
        blocked.outcome,
        crate::presentation::WorkRunControlOutcome::Consumed
    );
    assert_eq!(runtime.state().director_new(), DirectorNew::Idle);
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_some(),
        "the fence remains active after a Workflow route normalization"
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(!runtime.state().director_drawer_open());
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_some(),
        "a closed Director must not bypass the pending action fence"
    );
    assert!(!runtime.state().director_drawer_open());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let drawer = crate::presentation::director_drawer::geometry(20, 80);
    let start = Key::Click {
        column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
        row: u16::try_from(drawer.top + 2).unwrap(),
    };
    assert!(
        crate::presentation::open_director_from_new_button(
            &mut runtime,
            &start,
            20,
            80,
            control.mode(),
        )
        .is_none(),
        "the Start button is inert while a durable action is in flight"
    );
}

#[test]
fn focused_shell_outweighs_background_work_run_control() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let runs = crate::presentation::WorkRunProjection::fresh(vec![observed_work_run(
        SupervisorRunState::Running,
    )]);
    let mut control = crate::presentation::WorkRunControl::default();
    let _ = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    );
    let _ = control.handle(
        crate::presentation::WorkRunControlAction::Cancel,
        runs.runs(),
        true,
    );
    let submitted = control.handle(
        crate::presentation::WorkRunControlAction::Enter,
        runs.runs(),
        true,
    );
    assert!(submitted.into_request().is_some());

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_none(),
        "a background Work Run submission must not consume Shell New"
    );
    assert_eq!(
        control.mode(),
        crate::presentation::WorkRunControlMode::Submitting
    );
    let mut shell_control = crate::presentation::WorkRunControl::default();
    shell_control.open(runs.runs());
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut shell_control,
            &runs,
            &Key::Char('x'),
        )
        .is_none(),
        "an unfocused Director must not consume Shell input"
    );
    assert_eq!(
        shell_control.mode(),
        crate::presentation::WorkRunControlMode::Closed
    );
    assert_eq!(
        crate::presentation::workspace_foreground_input_owner(&runtime),
        crate::presentation::WorkspaceForegroundInputOwner::Downstream
    );
}

#[test]
#[allow(clippy::too_many_lines)] // This table drives every keyboard edge of the retained run surface.
fn work_run_routes_confirmations_and_console_activation_without_implicit_mutation() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let running = observed_work_run(SupervisorRunState::Running);
    let runs = crate::presentation::WorkRunProjection::fresh(vec![running.clone()]);
    let mut control = crate::presentation::WorkRunControl::default();
    let _ = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    );

    let _ = control.handle(
        crate::presentation::WorkRunControlAction::Cancel,
        runs.runs(),
        true,
    );
    for key in [Key::Up, Key::Down, Key::Left, Key::Right, Key::CtrlX] {
        let input = crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &key,
        )
        .unwrap();
        assert_eq!(
            input.outcome,
            crate::presentation::WorkRunControlOutcome::Consumed
        );
    }
    let _ = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Quit,
    );
    assert_eq!(
        control.mode(),
        crate::presentation::WorkRunControlMode::List
    );

    for key in [Key::Escape, Key::Live(LiveTerminalAction::DirectorBack)] {
        let _ = control.handle(
            crate::presentation::WorkRunControlAction::Cancel,
            runs.runs(),
            true,
        );
        let _ = crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &key,
        );
        assert_eq!(
            control.mode(),
            crate::presentation::WorkRunControlMode::List
        );
    }
    let _ = control.handle(
        crate::presentation::WorkRunControlAction::Cancel,
        runs.runs(),
        true,
    );
    let submitted = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Enter,
    )
    .unwrap();
    assert!(submitted.outcome.into_request().is_some());

    let mut empty_control = crate::presentation::WorkRunControl::default();
    let empty = crate::presentation::WorkRunProjection::fresh(Vec::new());
    empty_control.open(empty.runs());
    let _ = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut empty_control,
        &empty,
        &Key::Enter,
    );
    assert_eq!(empty_control.feedback(), Some("No Work Run is selected"));

    let mut inert_control = crate::presentation::WorkRunControl::default();
    inert_control.open(runs.runs());
    for route in [
        DirectorRoute::Organization,
        DirectorRoute::Console(DirectorConsoleParent::Organization),
    ] {
        let input = crate::presentation::handle_work_run_list_input(
            None,
            &mut runtime,
            &mut inert_control,
            &runs,
            &Key::Enter,
            route,
            Vec::new(),
        );
        assert_eq!(
            input.outcome,
            crate::presentation::WorkRunControlOutcome::Consumed
        );
    }
    let _ = crate::presentation::handle_work_run_list_input(
        None,
        &mut runtime,
        &mut inert_control,
        &runs,
        &Key::Quit,
        DirectorRoute::WorkRuns,
        Vec::new(),
    );
    assert_eq!(
        inert_control.mode(),
        crate::presentation::WorkRunControlMode::ConfirmCancel
    );

    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);
    let mut suspended = crate::presentation::WorkRunControl::default();
    suspended.open(runs.runs());
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut suspended,
            &runs,
            &Key::Other,
        )
        .is_none()
    );
    assert_eq!(
        suspended.mode(),
        crate::presentation::WorkRunControlMode::Closed
    );
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);

    let terminal = scoped_terminal_ref(workspace, None);
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Agent);
    let _ = runtime.complete_pane(Target::Root(workspace), operation, terminal.clone());
    let runtime_id = AgentRuntimeId::new();
    let continuation = AgentContinuationRef::new();
    let mut run_with_director = running;
    run_with_director.root_agent_id = Some(runtime_id);
    let run_id = run_with_director.supervisor_run_id;
    let overview_runs = crate::presentation::WorkRunProjection::fresh(vec![run_with_director]);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    ui.agent_inventory = Some(AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(runtime_id, terminal, None).unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    });
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorRunOverview(run_id)));
    let mut overview_control = crate::presentation::WorkRunControl::default();
    overview_control.open(overview_runs.runs());
    let activated = crate::presentation::handle_work_run_control_input_with_ui(
        Some(&mut ui),
        &mut runtime,
        &mut overview_control,
        &overview_runs,
        &Key::Enter,
    )
    .unwrap();
    assert!(activated.effects.is_empty());
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Console(DirectorConsoleParent::RunOverview(run_id))
    );
    assert!(
        crate::presentation::handle_work_run_control_input(
            &mut runtime,
            &mut overview_control,
            &overview_runs,
            &Key::Other,
        )
        .is_none(),
        "the Console owns input after leaving the Run Overview"
    );
    assert_eq!(
        overview_control.mode(),
        crate::presentation::WorkRunControlMode::Closed
    );

    let no_root = observed_work_run(SupervisorRunState::Running);
    let no_root_id = no_root.supervisor_run_id;
    let no_root_runs = crate::presentation::WorkRunProjection::fresh(vec![no_root]);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorRunOverview(no_root_id)));
    let mut unavailable_control = crate::presentation::WorkRunControl::default();
    unavailable_control.open(no_root_runs.runs());
    let _ = crate::presentation::handle_work_run_control_input(
        &mut runtime,
        &mut unavailable_control,
        &no_root_runs,
        &Key::Enter,
    );
    assert_eq!(
        unavailable_control.feedback(),
        Some("Director Console is not available for this Work Run")
    );
}

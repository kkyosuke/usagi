//! workspace の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn project_bar_click_closes_the_pr_modal_without_reaching_the_bar() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::PullRequestsLoaded {
        target: Target::Session(session),
        revision: 1,
        prs: vec![usagi_core::domain::pr_inventory::PrEntry::new(
            usagi_core::domain::pr_inventory::canonicalize(
                "https://github.com/kkyosuke/usagi/pull/1625",
            )
            .unwrap(),
        )],
    }));
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));

    assert!(dismiss_pr_modal_on_project_bar_click(
        &mut runtime,
        &Key::Click { column: 2, row: 0 }
    ));
    assert_eq!(runtime.state().overlay(), None);

    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));
    assert!(!dismiss_pr_modal_on_project_bar_click(
        &mut runtime,
        &Key::Click { column: 2, row: 1 }
    ));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));
}

#[test]
#[allow(clippy::too_many_lines)] // One shell fixture keeps port absence and async completion in sequence.
fn workspace_shell_harness_covers_port_absence_projection_and_async_launch_completion() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let terminal = live_terminal_ref(workspace, session);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));

    ui.start_terminal_session(terminal.clone(), Geometry { cols: 20, rows: 5 });
    ui.set_allowed_agent_sessions(BTreeSet::new());
    let allowed_sessions = BTreeSet::from([session]);
    ui.set_allowed_agent_sessions(allowed_sessions.iter().copied());
    ui.resize_terminals(Geometry { cols: 20, rows: 5 });
    assert!(ui.send_terminal_bytes(&terminal, b"x").is_err());
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(
        crate::presentation::session_name_for(&ui, session).as_deref(),
        Some("demo-session")
    );
    assert_eq!(
        crate::presentation::session_name_for(&ui, SessionId::new()),
        None
    );

    let records = ui.workspace.sessions().to_vec();
    crate::presentation::apply_session_projection(&mut ui, None, None, None, None, None);
    crate::presentation::apply_session_projection(
        &mut ui,
        Some(records.clone()),
        None,
        None,
        None,
        None,
    );
    assert!(ui.workspace.sessions().is_empty());
    assert!(ui.workspace.session_ids().is_empty());
    crate::presentation::apply_session_projection(
        &mut ui,
        Some(records),
        Some(vec![session]),
        None,
        None,
        None,
    );
    let records = ui.workspace.sessions().to_vec();
    crate::presentation::apply_session_projection(
        &mut ui,
        Some(records),
        Some(vec![session]),
        Some(std::collections::BTreeMap::new()),
        Some(std::collections::BTreeMap::new()),
        None,
    );
    let mut mismatched_runtime = WorkspaceRuntime::new(workspace, Vec::new());
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Terminal {
                operation: OperationId::new(),
                result: Err("late completion without an Agent port".to_owned()),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut mismatched_runtime,
        &mut std::collections::HashMap::new(),
        Geometry { cols: 20, rows: 5 },
    );
    crate::presentation::sync_runtime_sessions(&mut mismatched_runtime, &ui, &[]);
    let mut no_controls = LiveTerminalControls::default();
    let _ = crate::presentation::poll_and_project_terminals(
        &mut ui,
        &mut mismatched_runtime,
        &mut no_controls,
        Geometry { cols: 20, rows: 5 },
    );
    let mut ui = ui
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(SuccessfulAgentPort(terminal.clone())),
        )
        .with_pane_launch_port(launch_port(Box::new(SuccessfulAgentPort(terminal.clone()))));
    assert!(ui.send_terminal_bytes(&terminal, b"missing").is_err());
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    for session in [Some(session), None] {
        ui.pane_launches
            .push(crate::presentation::PaneLaunch::Agent {
                operation: OperationId::new(),
                workspace,
                session,
                profile: None,
                goal: None,
                resume: true,
            });
        crate::presentation::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
        drain_completions_at(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
            1,
            Geometry { cols: 20, rows: 5 },
        );
    }
    let operation = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: operation,
        profile: None,
    });
    ui.pane_launches
        .push(crate::presentation::PaneLaunch::Agent {
            operation,
            workspace,
            session: Some(session),
            profile: None,
            goal: None,
            resume: false,
        });
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    crate::presentation::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
    drain_completions_at(
        &mut ui,
        &mut runtime,
        &mut pending,
        1,
        Geometry { cols: 20, rows: 5 },
    );
    assert!(pending.is_empty());
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "a successful Agent launch must replace the pre-launch inventory"
    );
    ui.resize_terminals(Geometry { cols: 30, rows: 6 });
    let projected_records = ui.workspace.sessions().to_vec();
    crate::presentation::apply_session_projection(
        &mut ui,
        Some(projected_records),
        Some(vec![session]),
        None,
        Some(std::collections::BTreeMap::from([(
            session,
            usagi_core::domain::session_lifecycle::SessionLifecycleProjection {
                lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
                failure_stage: None,
                failure_summary: None,
            },
        )])),
        None,
    );

    let operation = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: operation,
        arguments: "new".into(),
    });
    ui.pane_launches
        .push(crate::presentation::PaneLaunch::Terminal {
            operation,
            workspace,
            session: Some(session),
            arguments: "new".into(),
        });
    pending.insert(operation, target);
    crate::presentation::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
    drain_completions_at(
        &mut ui,
        &mut runtime,
        &mut pending,
        1,
        Geometry { cols: 20, rows: 5 },
    );
    assert_eq!(
        runtime
            .state()
            .terminal_launch_error()
            .map(|notice| notice.message.as_str()),
        Some("terminal launch is unavailable")
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));

    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Agent {
                operation: OperationId::new(),
                result: Ok(AgentPaneAdmission {
                    terminal: terminal.clone(),
                    continuation: None,
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "a daemon admission still changes inventory after its pending tab closes"
    );

    let failed_agent = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: failed_agent,
        profile: None,
    });
    pending.insert(failed_agent, target);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Agent {
                operation: failed_agent,
                result: Err("safe Agent launch failure".to_owned()),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert!(pending.is_empty());
    assert!(
        !ui.take_agent_inventory_change_observation_request(),
        "a rejected Agent launch does not change daemon inventory"
    );
    assert_eq!(runtime.state().overlay(), Some(Overlay::AgentLaunchError));
    assert_eq!(
        runtime
            .state()
            .agent_launch_error()
            .map(|notice| notice.message.as_str()),
        Some("safe Agent launch failure")
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));

    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Terminal {
                operation: OperationId::new(),
                result: Err("late terminal failure".to_owned()),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );

    let failed_terminal = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: failed_terminal,
        arguments: "open".into(),
    });
    pending.insert(failed_terminal, target);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Terminal {
                operation: failed_terminal,
                result: Err("login shell could not be started".to_owned()),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert_eq!(
        runtime.state().overlay(),
        Some(Overlay::TerminalLaunchError)
    );
    assert_eq!(
        runtime
            .state()
            .terminal_launch_error()
            .map(|notice| notice.message.as_str()),
        Some("login shell could not be started")
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));

    let cancel = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: cancel,
        arguments: "open".into(),
    });
    ui.pane_launches
        .push(crate::presentation::PaneLaunch::Terminal {
            operation: cancel,
            workspace,
            session: Some(session),
            arguments: "open".into(),
        });
    pending.insert(cancel, target);
    let _ = runtime.select_tab(TabDirection::Next);
    crate::presentation::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending);

    // Two more requests: admission takes exactly one worker and leaves the
    // rest visibly pending, without ever touching the stream port.
    assert!(ui.poll_all_terminals().is_empty());
    let queued_before = ui.pane_launches.len();
    ui.pane_launches
        .push(crate::presentation::PaneLaunch::Agent {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            profile: None,
            goal: None,
            resume: false,
        });
    ui.pane_launches
        .push(crate::presentation::PaneLaunch::Terminal {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            arguments: "open".into(),
        });
    crate::presentation::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
    assert_eq!(ui.pane_launches.len(), queued_before + 1);
    assert!(ui.active_pane_launch.is_some());
    assert!(ui.poll_all_terminals().is_empty());
}

#[test]
fn a_workspace_exit_never_strands_the_shared_launch_client() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let launched = scoped_terminal_ref(workspace, Some(session));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished) = std::sync::mpsc::channel();
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(GatedLaunchPort {
            terminal: launched,
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            finished: Mutex::new(finished_tx),
        }),
    );
    crate::presentation::enqueue_pane_launch(
        &mut ui,
        agent_launch(workspace, session, OperationId::new()),
    );
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );

    // The workspace exits while the worker is still inside the client: its
    // completion receiver is gone, so the send is dropped harmlessly and the
    // borrowed client outlives the UI instead of being lost with it.
    drop(ui);
    release.send(()).unwrap();
    assert_eq!(
        finished.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    assert_eq!(stream.lock().unwrap().launches, 0);
}

#[test]
fn drawer_root_final_without_conversation_identity_fails_closed() {
    let workspace = WorkspaceId::new();
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_tab_intent(
        workspace,
        BTreeSet::new(),
        Box::new(MemoryIntentPort {
            state: Arc::clone(&durable),
            mutations: Arc::clone(&mutations),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let operation = OperationId::new();
    let target = Target::Root(workspace);
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: None,
        operation_id: operation,
        profile: Some(AgentProfileId::new("codex").unwrap()),
    });
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Agent {
                operation,
                result: Ok(AgentPaneAdmission {
                    terminal: scoped_terminal_ref(workspace, None),
                    continuation: None,
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );

    assert!(pending.is_empty());
    assert!(live_tab_terminals(&runtime, target).is_empty());
    assert!(durable.lock().unwrap().targets.is_empty());
    assert!(mutations.lock().unwrap().is_empty());
}

#[test]
fn workspace_shell_composition_keeps_home_below_the_project_bar() {
    let snapshot = snapshot("atlas");
    let deck = WorkspaceDeck::new(&snapshot);
    let home = (0..19)
        .map(|row| format!("home row {row}"))
        .collect::<Vec<_>>();

    let frame = compose_workspace_shell_frame(&deck, 19, 80, &home);

    assert_eq!(frame.len(), 20);
    assert!(strip_ansi(&frame[0]).contains("atlas"));
    assert_eq!(frame[1], "home row 0");
}

#[test]
fn workspace_exit_does_not_drop_the_admitted_effect_completion() {
    let snapshot = snapshot("demo");
    let workspace = snapshot.workspace_id;
    let session = snapshot.session_ids[0];
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(
        &command_lane,
        view,
        Box::new(BlockingSessionPort {
            existing: session,
            created: SessionId::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
            started: started_tx,
            release: Mutex::new(release_rx),
            block_once: AtomicBool::new(true),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (mut host, actions) = ControllerHost::channel();
    let completion = enqueue_session_request(
        &mut host,
        ConcurrentSessionRequest::Create(1),
        workspace,
        session,
    );
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    drop(ui);
    drop(runtime);
    drop(actions);
    release_tx.send(()).unwrap();

    assert!(matches!(
        completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::OperationResult(_)
    ));
    assert!(completion.try_recv().is_err());
}

#[test]
fn memory_intent_port_projects_a_stale_observation_without_mutating_state() {
    let workspace = WorkspaceId::new();
    let mut durable = AgentTabIntent::empty(workspace);
    durable.revision = 1;
    let mut port = MemoryIntentPort {
        state: Arc::new(Mutex::new(durable)),
        mutations: Arc::new(Mutex::new(Vec::new())),
    };
    let inventory = AgentInventory {
        workspace_id: workspace,
        runtimes: Vec::new(),
        resumable: Vec::new(),
    };

    let commit = port
        .mutate(
            workspace,
            0,
            AgentTabIntentMutation::Observe {
                terminals: Vec::new(),
                agents: inventory,
                allowed_sessions: BTreeSet::new(),
            },
        )
        .unwrap();
    assert!(commit.cas_conflict);
    assert!(!commit.mutation_applied);
    assert_eq!(commit.projection, Some(AgentTabProjection::default()));
}

#[test]
#[allow(clippy::too_many_lines)] // One production-order fixture observes every reserved and passthrough branch.
fn production_input_order_reserves_drawer_picker_before_root_agent_pty() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_agent = scoped_terminal_ref(workspace, None);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_context(
        workspace,
        vec![session],
        Box::new(RestoreInventoryPort {
            entries: vec![TerminalInventoryEntry {
                terminal: root_agent.clone(),
                kind: TerminalKind::Agent,
                live: true,
            }],
            fail: false,
            inputs: Arc::clone(&inputs),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
        DefaultModel::Claude,
    );
    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();

    // This is the same production ordering used by the frame loop: the
    // closed drawer leaves the resolved chord for the reducer, which opens
    // both drawer and picker without sending anything to the managed pane.
    let new = Key::Live(LiveTerminalAction::DirectorNew);
    assert_eq!(
        route_workspace_input_before_reducer(&mut ui, &mut runtime, &mut controls, &mut term, &new,),
        WorkspaceInputRoute::Unhandled
    );
    assert!(runtime.handle_key(new).is_empty());
    assert!(runtime.state().director_drawer_open());
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(DefaultModel::Claude)
    ));
    assert_eq!(runtime.focused_terminal(), Some(root_agent));

    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::DirectorNew),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::Director),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(!runtime.state().director_drawer_open());
    let reopen = Key::Live(LiveTerminalAction::DirectorNew);
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &reopen,
        ),
        WorkspaceInputRoute::Unhandled
    );
    assert!(runtime.handle_key(reopen).is_empty());
    for key in [Key::Down, Key::Up] {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &key,
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
    }
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert_eq!(runtime.state().director_new(), DirectorNew::Idle);
    assert!(inputs.lock().unwrap().is_empty());

    for key in [
        Key::Up,
        Key::Down,
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Live(LiveTerminalAction::NextTab),
    ] {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &key,
            ),
            WorkspaceInputRoute::Unhandled
        );
    }

    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Char('x'),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Enter,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(inputs.lock().unwrap().is_empty());

    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::DirectorNew),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    let WorkspaceInputRoute::Drawer(effects) = route_workspace_input_before_reducer(
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &Key::Enter,
    ) else {
        panic!("picker Enter must stay in the drawer");
    };
    assert!(matches!(
        effects.as_slice(),
        [Effect::LaunchAgent {
            session: None,
            profile: Some(profile),
            ..
        }] if profile.as_str() == "claude"
    ));
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(runtime.state().director_drawer_open());
    assert!(runtime.state().director_launching().is_some());
    assert!(inputs.lock().unwrap().is_empty());
}

/// `Esc` belongs to the drawer's selected root Agent — an agent CLI reads it
/// as its own interrupt — so the drawer keeps it only when no live
/// conversation can receive it. `Ctrl-O Ctrl-G` closes the drawer either way.
#[test]
fn drawer_escape_reaches_the_selected_root_agent_and_closes_only_without_one() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_agent = scoped_terminal_ref(workspace, None);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_context(
        workspace,
        vec![session],
        Box::new(RestoreInventoryPort {
            entries: vec![TerminalInventoryEntry {
                terminal: root_agent.clone(),
                kind: TerminalKind::Agent,
                live: true,
            }],
            fail: false,
            inputs: Arc::clone(&inputs),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();

    let open = Key::Live(LiveTerminalAction::Director);
    assert!(runtime.handle_key(open).is_empty());
    assert!(runtime.state().director_drawer_open());
    assert_eq!(runtime.focused_terminal(), Some(root_agent.clone()));
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    assert!(runtime.handle_key(Key::Enter).is_empty());

    // The live conversation owns Esc: it reaches the PTY once and the drawer
    // stays open.
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Forwarded
    );
    assert!(runtime.state().director_drawer_open());
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![(root_agent.clone(), vec![0x1b])]
    );

    // Closing stays reachable through the drawer's own chord.
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::Director),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(!runtime.state().director_drawer_open());
    assert_eq!(*inputs.lock().unwrap(), vec![(root_agent, vec![0x1b])]);

    // With no conversation to receive it, Esc keeps its drawer meaning.
    let empty_view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut empty_ui = io_runtime(empty_view, Box::new(UnavailableSessionCommandPort));
    let mut empty_runtime = WorkspaceRuntime::new(workspace, vec![session]);
    assert!(
        empty_runtime
            .handle_key(Key::Live(LiveTerminalAction::Director))
            .is_empty()
    );
    assert!(empty_runtime.state().director_drawer_open());
    assert_eq!(empty_runtime.focused_terminal(), None);
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut empty_ui,
            &mut empty_runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(!empty_runtime.state().director_drawer_open());
}

#[test]
fn a_block_selection_over_padding_stays_visible_in_the_projected_rows() {
    // Regression: agents draw space-padded, mostly-blank screens. A block
    // drag across text, a blank line, and trailing padding must reach the
    // projected rows as reverse-video, not be trimmed into an invisible
    // selection (copy already worked from the snapshot cells).
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 9,
            replay: b"ab\r\n\r\ncd".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let cells = ui.terminal_cells(&terminal).expect("attached cells");
    let mut selection = TerminalSelection::begin(cells, TerminalPoint { row: 0, column: 0 });
    selection.extend(TerminalPoint { row: 1, column: 5 });
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));
    controls.begin_selection(selection);
    let rows = controller_terminal_view(&ui, &runtime, &mut controls, 10)
        .expect("selection view")
        .rows;
    // Row 0's trailing padding and the blank row 1 are highlighted.
    assert!(
        rows[0].contains("\u{1b}[7m") && rows[0].contains("ab"),
        "row 0 padding not highlighted: {:?}",
        rows[0]
    );
    assert!(
        rows[1].contains("\u{1b}[7m"),
        "blank row 1 not highlighted: {:?}",
        rows[1]
    );
}

#[test]
fn a_selection_over_long_history_keeps_the_projection_viewport_bounded() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let replay = (0..1_000)
        .flat_map(|line| format!("line {line}\r\n").into_bytes())
        .collect();
    let (ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 10,
            replay,
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let cells = ui.terminal_cells(&terminal).expect("attached cells");
    let last_row = cells.len().saturating_sub(1);
    let mut selection = TerminalSelection::begin(
        cells,
        TerminalPoint {
            row: last_row.saturating_sub(100),
            column: 0,
        },
    );
    selection.extend(TerminalPoint {
        row: last_row,
        column: 2,
    });
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));
    controls.begin_selection(selection);

    let viewport_rows = 20;
    let view = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
        .expect("selection view");
    assert!(view.total_rows > viewport_rows);
    assert_eq!(view.rows.len(), viewport_rows);
    assert_eq!(view.row_offset + view.rows.len(), view.total_rows);

    controls.scroll_up();
    let scrolled = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
        .expect("scrolled selection view");
    assert_eq!(scrolled.rows.len(), viewport_rows);
    assert_eq!(scrolled.scroll, 1);
    assert_eq!(
        scrolled.row_offset + scrolled.rows.len() + scrolled.scroll,
        scrolled.total_rows
    );
}

#[test]
fn workspace_switch_progress_appears_only_after_the_grace_or_cancellation() {
    assert!(!workspace_loading_visible(
        true,
        false,
        WORKSPACE_SWITCH_LOADING_GRACE
            .checked_sub(std::time::Duration::from_millis(1))
            .expect("the loading grace exceeds one millisecond"),
    ));
    assert!(workspace_loading_visible(
        true,
        false,
        WORKSPACE_SWITCH_LOADING_GRACE,
    ));
    assert!(workspace_loading_visible(
        true,
        true,
        std::time::Duration::ZERO,
    ));
    assert!(workspace_loading_visible(
        false,
        false,
        std::time::Duration::ZERO,
    ));
}

#[test]
fn missing_workspace_prompt_summarizes_multiple_paths() {
    let prompt = MissingWorkspacePrompt::new(vec!["/tmp/alpha".into(), "/tmp/beta".into()]);
    let frame = render_missing_workspace_prompt(24, 80, &vec![String::new(); 24], &prompt);
    let rendered = strip_ansi(&frame.join("\n"));

    assert!(rendered.contains("2 workspace directories no longer exist"));
    assert!(rendered.contains("Remove their registry entries?"));
}

#[test]
fn missing_workspace_prompt_keyboard_controls_are_complete() {
    let alpha = ws("alpha");
    let mut cancel_term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Right,
        Key::Enter,
        Key::Quit,
    ]);
    let mut cancel_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut cancel_term,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut cancel_loader,
    )
    .unwrap();
    assert_eq!(cancel_loader.cleanup_calls, 0);

    let mut enter_term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Enter, Key::Quit]);
    let mut enter_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut enter_term,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut enter_loader,
    )
    .unwrap();
    assert_eq!(enter_loader.cleanup_calls, 1);

    let mut quit_term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('x'), Key::Quit]);
    let mut quit_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    assert_eq!(
        run(
            &mut quit_term,
            vec![alpha],
            Vec::new(),
            now(),
            &mut quit_loader,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(quit_loader.cleanup_calls, 0);
}

#[test]
fn quitting_from_a_recent_workspace_exits_the_runtime() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::CtrlQ, Key::Char('y')]);
    run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert_eq!(term.frames.len(), 3);
    assert!(term.frames[1].join("\n").contains("recent-session"));
}

#[test]
#[allow(clippy::too_many_lines)] // One exhaustive surface-to-help table is easiest to audit intact.
fn workspace_help_resolver_covers_every_frontmost_surface() {
    use crate::presentation::views::key_help::Context as HelpContext;
    use crate::presentation::{
        WorkspaceBaseHelp, WorkspaceDeckHelp, WorkspaceHelpState, resolve_workspace_help_context,
    };

    let base = WorkspaceHelpState {
        deck: WorkspaceDeckHelp::None,
        overlay: None,
        decision_answer_open: false,
        work_run_mode: crate::presentation::WorkRunControlMode::Closed,
        director_new_open: false,
        director_route: DirectorRoute::Organization,
        drawer_focus: None,
        base: WorkspaceBaseHelp::Switch,
    };
    assert_eq!(
        WorkspaceDeckHelp::new(false, false),
        WorkspaceDeckHelp::None
    );
    assert_eq!(
        WorkspaceDeckHelp::new(true, true),
        WorkspaceDeckHelp::AddWorkspace
    );
    assert_eq!(
        WorkspaceDeckHelp::new(false, true),
        WorkspaceDeckHelp::WorkspaceFinder
    );
    assert_eq!(
        WorkspaceBaseHelp::new(Route::Home(HomeMode::Switch), false),
        WorkspaceBaseHelp::Switch
    );
    assert_eq!(
        WorkspaceBaseHelp::new(Route::Home(HomeMode::Closeup), false),
        WorkspaceBaseHelp::Closeup
    );
    assert_eq!(
        WorkspaceBaseHelp::new(Route::Home(HomeMode::Closeup), true),
        WorkspaceBaseHelp::LiveTerminal
    );
    assert_eq!(resolve_workspace_help_context(base), HelpContext::Switch);
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            deck: WorkspaceDeckHelp::AddWorkspace,
            ..base
        }),
        HelpContext::AddWorkspace
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            deck: WorkspaceDeckHelp::WorkspaceFinder,
            ..base
        }),
        HelpContext::WorkspaceFinder
    );

    for (overlay, expected) in [
        (Overlay::Overview, HelpContext::Overview),
        (Overlay::Daemon, HelpContext::Daemon),
        (Overlay::Closeup, HelpContext::CloseupActions),
        (Overlay::QuitConfirmation, HelpContext::ExitConfirmation),
        (Overlay::ForceRemoveConfirmation, HelpContext::ForceRemove),
        (Overlay::Notes, HelpContext::Scratchpad),
        (
            Overlay::Environment,
            HelpContext::WorkspaceEnvironmentEditor,
        ),
        (Overlay::Roles, HelpContext::RolesEditor),
        (Overlay::CreateSession, HelpContext::CreateSession),
        (Overlay::Decisions, HelpContext::DecisionList),
        (Overlay::CleanupQueue, HelpContext::CleanupQueue),
        (Overlay::RemoveSessions, HelpContext::RemoveSessions),
        (Overlay::Prs, HelpContext::PullRequests),
        (Overlay::Preview, HelpContext::Preview),
        (Overlay::CreateSessionError, HelpContext::CreateSessionError),
        (
            Overlay::TerminalLaunchError,
            HelpContext::TerminalLaunchError,
        ),
        (Overlay::AgentLaunchError, HelpContext::AgentLaunchError),
        (Overlay::Garden, HelpContext::Garden),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                overlay: Some(overlay),
                ..base
            }),
            expected,
            "{overlay:?}"
        );
    }
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            overlay: Some(Overlay::Decisions),
            decision_answer_open: true,
            ..base
        }),
        HelpContext::DecisionAnswer
    );

    for (mode, expected) in [
        (
            crate::presentation::WorkRunControlMode::List,
            HelpContext::WorkRuns,
        ),
        (
            crate::presentation::WorkRunControlMode::ResolveEscalation,
            HelpContext::WorkRunEscalation,
        ),
        (
            crate::presentation::WorkRunControlMode::ConfirmCancel,
            HelpContext::WorkRunConfirmation,
        ),
        (
            crate::presentation::WorkRunControlMode::Submitting,
            HelpContext::WorkRunSubmitting,
        ),
        (
            crate::presentation::WorkRunControlMode::Retry,
            HelpContext::WorkRunConfirmation,
        ),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                work_run_mode: mode,
                director_route: DirectorRoute::WorkRuns,
                drawer_focus: Some(WorkspaceDrawerFocus::Director),
                ..base
            }),
            expected,
            "{mode:?}"
        );
    }
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: crate::presentation::WorkRunControlMode::List,
            director_route: DirectorRoute::RunOverview(SupervisorRunId::new()),
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::RunOverview
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: crate::presentation::WorkRunControlMode::ConfirmDelete,
            director_route: DirectorRoute::WorkRuns,
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::WorkRunConfirmation
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: crate::presentation::WorkRunControlMode::Submitting,
            director_route: DirectorRoute::Organization,
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::WorkRunSubmitting,
        "an in-flight action outranks the normalized Organization route"
    );

    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            director_new_open: true,
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::DirectorNew
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: crate::presentation::WorkRunControlMode::Submitting,
            director_new_open: true,
            director_route: DirectorRoute::WorkRuns,
            drawer_focus: Some(WorkspaceDrawerFocus::Terminal),
            ..base
        }),
        HelpContext::RootShell,
        "the focused Shell outranks a background Director operation"
    );
    for (drawer_focus, expected) in [
        (WorkspaceDrawerFocus::Director, HelpContext::Organization),
        (WorkspaceDrawerFocus::Terminal, HelpContext::RootShell),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                drawer_focus: Some(drawer_focus),
                ..base
            }),
            expected
        );
    }
    for (director_route, expected) in [
        (
            DirectorRoute::Console(DirectorConsoleParent::Organization),
            HelpContext::DirectorConsole,
        ),
        (
            DirectorRoute::Console(DirectorConsoleParent::RunOverview(SupervisorRunId::new())),
            HelpContext::WorkRunConsole,
        ),
        (
            DirectorRoute::RunOverview(SupervisorRunId::new()),
            HelpContext::RunOverview,
        ),
        (DirectorRoute::WorkRuns, HelpContext::WorkRuns),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                director_route,
                drawer_focus: Some(WorkspaceDrawerFocus::Director),
                ..base
            }),
            expected
        );
    }
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            base: WorkspaceBaseHelp::Closeup,
            ..base
        }),
        HelpContext::Closeup
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            base: WorkspaceBaseHelp::LiveTerminal,
            ..base
        }),
        HelpContext::LiveTerminal
    );
}

#[test]
fn workspace_help_describes_switch_and_swallows_background_commands() {
    use crate::presentation::{KeyHelpContext, closes_workspace_help};

    assert!(closes_workspace_help(
        &Key::Char('?'),
        KeyHelpContext::Closeup
    ));
    assert!(!closes_workspace_help(
        &Key::Char('?'),
        KeyHelpContext::LiveTerminal
    ));

    let mut term = FakeTerminal::with_keys(&[
        Key::Char('1'),
        Key::Char('?'),
        // Ctrl-X would remove the selected session outside Help.
        Key::CtrlX,
        Key::Char('?'),
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();

    let help = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .find(|frame| frame.contains("Keyboard help · Workspace switch"))
        .expect("workspace Help frame");
    assert!(help.contains("Ctrl-X"));
    assert!(help.contains("force remove session"));
    assert!(!help.contains("Available"));
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("recent-session")),
        "the command pressed behind Help must not remove the session"
    );
}

#[test]
fn workspace_loader_failure_is_propagated() {
    for (keys, recent) in [
        (vec![Key::Char('o'), Key::Enter], Vec::new()),
        (vec![Key::Char('1')], vec![recent("alpha")]),
    ] {
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            fail: true,
            ..FakeLoader::default()
        };
        let error = run(&mut term, vec![ws("alpha")], recent, now(), &mut loader).unwrap_err();
        assert_eq!(error.to_string(), "open failed");
    }
}

/// #556 acceptance: leaving tears the workspace down. Every resident port of
/// the first composition — including the session catalog — is dropped before
/// the second composition exists. Detached restore and branch adapters have an
/// explicitly separate lifetime and own no resident connection.
#[test]
fn leaving_a_workspace_drops_every_port_before_the_next_one_is_created() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('w'),
        Key::Char('2'),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        opened_at: Some(now() + Duration::hours(1)),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        Vec::new(),
        vec![
            recent_at("first", now()),
            recent_at("second", now() - Duration::hours(1)),
        ],
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    // Exactly two compositions, and the second one started with the first
    // one already fully torn down: residue would show as a shortfall here.
    assert_eq!(
        factory.drops_at_create,
        vec![0, RESIDENT_PORTS_PER_COMPOSITION]
    );
    // After the run, the second composition is gone too: nothing outlives it.
    assert_eq!(
        factory.drops.load(Ordering::SeqCst),
        2 * RESIDENT_PORTS_PER_COMPOSITION
    );
}

#[test]
fn project_deck_add_and_digit_switch_drop_the_old_composition_before_create() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Live(LiveTerminalAction::ActivateWorkspace(1)),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
        ]
    );
    assert_eq!(
        factory.drops_at_create,
        vec![
            0,
            RESIDENT_PORTS_PER_COMPOSITION,
            2 * RESIDENT_PORTS_PER_COMPOSITION,
        ]
    );
}

#[test]
fn add_workspace_overlay_can_close_its_checked_active_project() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::CtrlX,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(factory.drops_at_create, vec![0]);
    assert!(term.frames.last().unwrap().join("\n").contains("Recent"));
}

#[test]
fn project_arrows_follow_tab_order_and_ctrl_option_reaches_closeup() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Left,
        Key::Right,
        Key::Right,
        Key::Enter,
        Key::Live(LiveTerminalAction::NextWorkspace),
        Key::Live(LiveTerminalAction::PreviousWorkspace),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
        ]
    );
    assert_eq!(
        factory.drops_at_create,
        vec![
            0,
            RESIDENT_PORTS_PER_COMPOSITION,
            2 * RESIDENT_PORTS_PER_COMPOSITION,
            3 * RESIDENT_PORTS_PER_COMPOSITION,
            4 * RESIDENT_PORTS_PER_COMPOSITION,
            5 * RESIDENT_PORTS_PER_COMPOSITION,
            6 * RESIDENT_PORTS_PER_COMPOSITION,
        ]
    );
}

#[test]
fn project_navigation_chords_reach_closeup_but_yield_to_foreground_surfaces() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let gamma = snapshot("gamma");
    let deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta, gamma]).unwrap();
    let mut state = AppState::home(alpha.workspace_id, alpha.session_ids.clone());

    assert_eq!(
        crate::presentation::workspace_navigation_target(&deck, &state, &Key::Left),
        Some(PathBuf::from("/tmp/gamma"))
    );
    assert_eq!(
        crate::presentation::workspace_navigation_target(&deck, &state, &Key::Right),
        Some(PathBuf::from("/tmp/beta"))
    );
    assert_eq!(
        crate::presentation::workspace_navigation_target(&deck, &state, &Key::Up),
        None
    );

    let _ =
        crate::usecase::application::controller::update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(
        crate::presentation::workspace_navigation_target(&deck, &state, &Key::Right),
        None
    );
    assert_eq!(
        crate::presentation::workspace_navigation_target(
            &deck,
            &state,
            &Key::Live(LiveTerminalAction::PreviousWorkspace),
        ),
        Some(PathBuf::from("/tmp/gamma"))
    );
    assert_eq!(
        crate::presentation::workspace_navigation_target(
            &deck,
            &state,
            &Key::Live(LiveTerminalAction::NextWorkspace),
        ),
        Some(PathBuf::from("/tmp/beta"))
    );

    let mut state = AppState::home(alpha.workspace_id, alpha.session_ids.clone());
    let _ =
        crate::usecase::application::controller::update(&mut state, AppEvent::Key(AppKey::CtrlQ));
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
    assert_eq!(
        crate::presentation::workspace_navigation_target(
            &deck,
            &state,
            &Key::Live(LiveTerminalAction::PreviousWorkspace),
        ),
        None
    );

    let mut overlay_deck = deck;
    overlay_deck.open_switcher();
    let state = AppState::home(alpha.workspace_id, alpha.session_ids);
    assert_eq!(
        crate::presentation::workspace_navigation_target(
            &overlay_deck,
            &state,
            &Key::Live(LiveTerminalAction::NextWorkspace),
        ),
        None
    );
}

#[test]
fn project_bar_click_adds_and_activates_the_project_identity_it_rendered() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Click { column: 10, row: 0 },
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Click { column: 2, row: 0 },
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    term.size = Some((24, 80));
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha"), ws("beta")],
        Vec::new(),
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
        ]
    );
    assert!(term.frames.iter().any(|frame| frame[0].contains("+ Open")));
}

#[test]
fn project_prepare_failure_keeps_the_current_composition_and_deck() {
    const REFUSAL: &str = "another daemon owns beta; retry after it releases the workspace";
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        refuse_paths: vec![PathBuf::from("/tmp/beta")],
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha"), ws("beta")],
        Vec::new(),
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    assert_eq!(
        loader.opened,
        vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")]
    );
    assert_eq!(factory.drops_at_create, vec![0]);
}

#[test]
fn project_batch_settings_failure_is_all_or_nothing() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort {
        refuse: Some(PathBuf::from("/tmp/gamma")),
        ..WorkspaceBindingSettingsPort::default()
    };
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha"), ws("beta"), ws("gamma")],
        Vec::new(),
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/gamma"),
        ]
    );
    assert_eq!(factory.drops_at_create, vec![0]);
    assert!(settings.selected.ends_with(&[
        PathBuf::from("/tmp/beta"),
        PathBuf::from("/tmp/gamma"),
        PathBuf::from("/tmp/alpha"),
    ]));
}

#[test]
fn project_registry_path_fence_covers_empty_match_and_mismatch() {
    let alpha = snapshot("alpha");
    assert!(!registry_contains_path(&[], &alpha.workspace.path));
    assert!(registry_contains_path(
        std::slice::from_ref(&alpha.workspace),
        &alpha.workspace.path,
    ));
    assert!(!registry_contains_path(
        std::slice::from_ref(&alpha.workspace),
        Path::new("/tmp/beta"),
    ));

    let beta = snapshot("beta");
    let mut registry = vec![alpha.workspace, beta.workspace.clone()];
    remove_registry_paths(&mut registry, &[]);
    remove_registry_paths(&mut registry, &[PathBuf::from("/tmp/missing")]);
    remove_registry_paths(&mut registry, &[PathBuf::from("/tmp/alpha")]);
    assert_eq!(registry, vec![beta.workspace]);
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers the deck helpers' shared fallback state.
fn project_deck_composition_helpers_cover_safe_fallbacks() {
    let alpha = snapshot("alpha");
    assert!(!workspace_has_unsaved_surface(&WorkspaceRuntime::new(
        alpha.workspace_id,
        Vec::new(),
    )));
    let mut deck = WorkspaceDeck::new(&alpha);
    let mut term = FakeTerminal::default();

    let mut no_loader: Option<&mut dyn WorkspaceLoader> = None;
    restore_prepared_workspace(&mut no_loader, Path::new("/tmp/alpha"));
    assert!(
        prepare_deck_workspace(
            &mut term,
            &mut no_loader,
            &mut deck,
            Path::new("/tmp/beta"),
            "Opening…",
        )
        .is_none()
    );
    assert!(deck.notice().unwrap().contains("workspace list"));

    let mut successful_loader = FakeLoader::default();
    let mut loader: Option<&mut dyn WorkspaceLoader> = Some(&mut successful_loader);
    assert_eq!(
        prepare_deck_workspace(
            &mut term,
            &mut loader,
            &mut deck,
            Path::new("/tmp/beta"),
            "Opening…",
        )
        .unwrap()
        .workspace
        .path,
        PathBuf::from("/tmp/beta")
    );

    let mut failed_loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    let mut loader: Option<&mut dyn WorkspaceLoader> = Some(&mut failed_loader);
    assert!(
        prepare_deck_workspace(
            &mut term,
            &mut loader,
            &mut deck,
            Path::new("/tmp/beta"),
            "Opening…",
        )
        .is_none()
    );
    assert_eq!(deck.notice(), Some("open failed"));

    let mut no_config = None;
    assert!(prepare_activation_settings(
        &mut no_config,
        &mut no_loader,
        &mut deck,
        Path::new("/tmp/alpha"),
        Path::new("/tmp/beta"),
    ));
    assert!(prepare_batch_settings(
        &mut no_config,
        &mut no_loader,
        &mut deck,
        Path::new("/tmp/alpha"),
        &[],
    ));

    let mut settings = WorkspaceBindingSettingsPort {
        refuse: Some(PathBuf::from("/tmp/beta")),
        ..WorkspaceBindingSettingsPort::default()
    };
    let mut rollback_loader = FakeLoader::default();
    let mut rollback: Option<&mut dyn WorkspaceLoader> = Some(&mut rollback_loader);
    let mut context = Some(WorkspaceConfigContext {
        settings: &mut settings,
        available_models: AvailableAgentModels::all(),
    });
    assert!(!prepare_activation_settings(
        &mut context,
        &mut rollback,
        &mut deck,
        Path::new("/tmp/alpha"),
        Path::new("/tmp/beta"),
    ));
    assert_eq!(deck.notice(), Some("workspace settings are unreadable"));

    assert_eq!(
        adjust_project_bar_pointer(Key::Click { column: 4, row: 2 }),
        Key::Click { column: 4, row: 1 }
    );
    assert_eq!(
        adjust_project_bar_pointer(Key::Click { column: 4, row: 0 }),
        Key::Click { column: 4, row: 0 }
    );
    assert_eq!(
        adjust_project_bar_pointer(Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 4,
            row: 2,
        })),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 4,
            row: 1,
        })
    );
    assert_eq!(adjust_project_bar_pointer(Key::Other), Key::Other);

    assert_eq!(
        prepare_workspace_deck(&mut term, &mut FakeLoader::default(), &[])
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        recent_paths(&Recent::Workspace(WorkspaceOverview::new(
            alpha.workspace,
            0,
            0,
            0,
        ))),
        vec![PathBuf::from("/tmp/alpha")]
    );
    assert!(recent_paths(&Recent::Unite(UniteOverview::new(Vec::new()))).is_empty());
}

/// A workspace opened directly (`usagi open <path>`) has no Welcome behind it, so
/// the runner reports the choice and the composition root decides. Quitting
/// and leaving must be different answers here too.
#[test]
fn a_direct_workspace_reports_leaving_and_quitting_as_different_exits() {
    for (key, expected) in [
        (Key::Char('w'), Exit::Welcome),
        (Key::Char('q'), Exit::Quit),
    ] {
        let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, key.clone()]);
        let mut factory = FixedBackendFactory {
            sessions: Some(Box::new(UnavailableSessionCommandPort)),
            agent: Some(Box::new(UnavailableAgentCommandPort)),
            launch: None,
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
            session_refresh: None,
            decisions: None,
            session_worktrees: None,
        };

        assert_eq!(
            run_workspace_controller_with_backend(&mut term, snapshot("direct"), &mut factory,)
                .unwrap(),
            expected,
            "{key:?}"
        );
    }
}

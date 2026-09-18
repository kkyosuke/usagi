//! terminal の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn app_event_from_key_maps_resolved_live_actions_to_reducer_keys() {
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::KeyboardHelp)),
        None
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::Switch)),
        Some(AppEvent::Key(AppKey::CtrlO))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenCloseupModal)),
        Some(AppEvent::Key(AppKey::OpenCloseupOverlay))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::PreviousSession)),
        Some(AppEvent::Key(AppKey::PreviousSession))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::NextSession)),
        Some(AppEvent::Key(AppKey::NextSession))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::NextTab)),
        Some(AppEvent::Key(AppKey::CtrlN))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::PreviousTab)),
        Some(AppEvent::Key(AppKey::CtrlP))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenPullRequests)),
        Some(AppEvent::Key(AppKey::OpenPrs))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenPreview)),
        Some(AppEvent::Key(AppKey::OpenPreview))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenDecisions)),
        Some(AppEvent::Key(AppKey::OpenDecisions))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenNotes)),
        Some(AppEvent::Key(AppKey::OpenNotes))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenGarden)),
        Some(AppEvent::Key(AppKey::OpenGarden))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::Agent)),
        Some(AppEvent::Key(AppKey::CtrlA))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::Director)),
        Some(AppEvent::Key(AppKey::ToggleDirectorDrawer))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::DirectorBack)),
        Some(AppEvent::Key(AppKey::DirectorBack))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::DirectorNew)),
        Some(AppEvent::Key(AppKey::OpenDirectorNew))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::WorkRuns)),
        Some(AppEvent::Key(AppKey::OpenDirectorWorkRuns))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::RootTerminal)),
        Some(AppEvent::Key(AppKey::ToggleRootTerminalDrawer))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::RootTerminalFullHeight)),
        Some(AppEvent::Key(AppKey::ToggleRootTerminalFullHeight))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::QuitConfirmation)),
        Some(AppEvent::Key(AppKey::OpenQuitConfirmation))
    );
}

#[test]
fn app_event_from_key_ticks_on_wakeups_and_drops_pane_only_input() {
    // Resize / backend wakeups reach the loop as `Other` and advance the mascot.
    assert_eq!(app_event_from_key(Key::Other), Some(AppEvent::Tick));
    // Raw passthrough and terminal pointer drags never reach the Home reducer.
    assert_eq!(app_event_from_key(Key::Passthrough(vec![0x1b])), None);
    // Sidebar clicks need the real runtime's injected monotonic timestamp.
    assert_eq!(app_event_from_key(Key::Click { column: 3, row: 4 }), None);
    // Left/Right reach the reducer to move the Yes/No confirmation focus; the
    // reducer ignores them outside that overlay. Ctrl-D stays Open-only.
    assert_eq!(
        app_event_from_key(Key::Left),
        Some(AppEvent::Key(AppKey::Left))
    );
    assert_eq!(
        app_event_from_key(Key::Right),
        Some(AppEvent::Key(AppKey::Right))
    );
    assert_eq!(app_event_from_key(Key::CtrlD), None);
    assert_eq!(app_event_from_key(Key::Help), None);
    // Tab close and terminal scroll/copy stay pane- and shell-level concerns.
    for action in [
        LiveTerminalAction::CloseTab,
        LiveTerminalAction::ScrollUp,
        LiveTerminalAction::ScrollDown,
        LiveTerminalAction::ScrollBottom,
    ] {
        assert_eq!(app_event_from_key(Key::Live(action)), None);
    }
    let terminal_copy_event = app_event_from_key(Key::TerminalCopy { fallback: vec![3] });
    #[cfg(target_os = "windows")]
    assert_eq!(terminal_copy_event, Some(AppEvent::Key(AppKey::CtrlC)));
    #[cfg(not(target_os = "windows"))]
    assert_eq!(terminal_copy_event, None);
}

#[test]
#[allow(clippy::too_many_lines)] // One host fixture verifies the ordered action-routing contract.
fn controller_host_executor_routes_busy_launch_terminal_and_tab_actions() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    );
    let token = PendingToken::from_raw(90);

    for effect in [
        Effect::CreateSession {
            workspace,
            token,
            operation_id: OperationId::new(),
            intent: SessionCreateIntent {
                name: "feature".into(),
                base_ref: None,
                profile: None,
                model: None,
                role_id: None,
            },
        },
        Effect::RefreshSessions { workspace },
        Effect::RemoveSession {
            workspace,
            session: SessionId::new(),
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        },
        Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: OperationId::new(),
            profile: None,
        },
        Effect::ResumeAgent {
            workspace,
            session,
            operation_id: OperationId::new(),
        },
        Effect::ReopenAgent {
            workspace: WorkspaceId::new(),
            continuation: AgentContinuationRef::new(),
        },
        Effect::OpenTerminal {
            target,
            operation_id: OperationId::new(),
            arguments: "new".into(),
        },
        Effect::OpenExternalTerminal { target },
        Effect::OpenExternalTerminal {
            target: Target::Session(SessionId::new()),
        },
        Effect::SelectTab {
            direction: TabDirection::Previous,
        },
    ] {
        backend.dispatch(effect);
    }
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    let completed = (0..1)
        .map(|_| {
            command_lane
                .completions
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("session command completion")
        })
        .collect::<Vec<_>>();
    for completion in completed {
        ui.session_completion_sender.send(completion).unwrap();
    }
    crate::presentation::drain_session_completions(&mut ui, &mut command_lane);
    let events = backend.drain_events();
    // Create is admitted and reports its token; Remove is refused as busy
    // and notices. `RefreshSessions` no longer competes for that single
    // command slot at all — it parks on the resident lane, which observes
    // nothing here, so it contributes no event (#551).
    assert_eq!(events.len(), 2);
    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::OperationResult(result) if result.token == token && !result.succeeded
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AppEvent::Backend(BackendEvent::Notice(_))))
            .count(),
        1
    );
    assert_eq!(ui.pane_launches.len(), 2);
    assert!(!pending.is_empty());

    let calls = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime_on(
        &command_lane,
        view,
        Box::new(SnapshotSessionPort(calls.clone())),
    )
    .with_agent_context(
        workspace,
        vec![session],
        Box::new(SuccessfulAgentPort(live_terminal_ref(workspace, session))),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    );
    backend.dispatch(Effect::RefreshSessions { workspace });
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    std::thread::sleep(std::time::Duration::from_millis(10));
    crate::presentation::drain_session_completions(&mut ui, &mut command_lane);
    backend.dispatch(Effect::SleepSession { workspace, session });
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    std::thread::sleep(std::time::Duration::from_millis(10));
    crate::presentation::drain_session_completions(&mut ui, &mut command_lane);
    backend.dispatch(Effect::RemoveSession {
        workspace,
        session,
        force: true,
        force_delete_branch: false,
        purge_orphan: false,
    });
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    std::thread::sleep(std::time::Duration::from_millis(10));
    crate::presentation::drain_session_completions(&mut ui, &mut command_lane);
    backend.dispatch(Effect::SleepSession {
        workspace,
        session: SessionId::new(),
    });
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    backend.dispatch(Effect::OpenTerminal {
        target,
        operation_id: OperationId::new(),
        arguments: "new".into(),
    });
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    // Only the user-initiated `Sleep` and `Remove` reach the command port. The
    // refresh went to the resident lane, so it neither spawned a worker nor
    // opened a connection of its own (#551).
    assert_eq!(
        calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2
    );
}

#[test]
fn right_pane_click_selection_reaches_the_runtime_tab_owner() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    let first_operation = OperationId::new();
    let first = live_terminal_ref(workspace, session);
    let _ = runtime.request_pane(target, first_operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(target, first_operation, first.clone());
    let second_operation = OperationId::new();
    let second = live_terminal_ref(workspace, session);
    let _ = runtime.request_pane(target, second_operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(target, second_operation, second);
    // A stale/out-of-range frame hit is inert.
    select_right_pane_tab(&mut ui, &mut runtime, usize::MAX);
    select_right_pane_tab(&mut ui, &mut runtime, 0);

    assert_eq!(runtime.focused_terminal(), Some(first));
}

#[test]
fn right_pane_agent_click_commits_intent_before_selection_and_surfaces_failure() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let first = live_terminal_ref(workspace, session);
    let second = live_terminal_ref(workspace, session);
    let continuation = AgentContinuationRef::new();
    let interrupted = interrupted_history(workspace, Some(session), true);
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: first.clone(),
        select: true,
    });
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: interrupted.continuation,
        terminal: interrupted.last_terminal.clone(),
        select: false,
    });
    let durable = Arc::new(Mutex::new(intent.clone()));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    for terminal in [first, second.clone()] {
        let operation = OperationId::new();
        let _ = runtime.request_pane(target, operation, PaneKind::Agent);
        let _ = runtime.complete_pane(target, operation, terminal);
    }
    runtime.inject_pane_event_for_test(
        target,
        crate::usecase::application::pane::PaneEvent::RestoreInterrupted {
            tabs: vec![interrupted.clone()],
        },
    );
    let pending = OperationId::new();
    let _ = runtime.request_pane(target, pending, PaneKind::Agent);

    let interrupted_index = runtime
            .active_pane()
            .tabs()
            .iter()
            .position(|tab| {
                matches!(tab, PaneTab::Interrupted(pane) if pane.tab.continuation == interrupted.continuation)
            })
            .expect("interrupted tab is visible");
    let pending_index = runtime
        .active_pane()
        .tabs()
        .iter()
        .position(|tab| matches!(tab, PaneTab::Pending(pane) if pane.operation == pending))
        .expect("pending tab is visible");
    select_right_pane_tab(&mut ui, &mut runtime, pending_index);
    select_right_pane_tab(&mut ui, &mut runtime, interrupted_index);

    select_right_pane_tab(&mut ui, &mut runtime, 1);

    assert_eq!(runtime.focused_terminal(), Some(second.clone()));
    assert!(mutations.lock().unwrap().iter().any(|mutation| matches!(
        mutation,
        AgentTabIntentMutation::Select {
            session_id: Some(selected),
            continuation: None,
        } if *selected == session
    )));

    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut failing_ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::new(Mutex::new(intent)),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    select_right_pane_tab(&mut failing_ui, &mut runtime, 0);

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.focused_terminal(), Some(second));
}

#[test]
#[allow(clippy::too_many_lines)] // One barrier fixture keeps both panes' IO in sequence.
fn a_blocked_pane_launch_keeps_every_live_pane_streaming() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let first = live_terminal_ref(workspace, session);
    let second = live_terminal_ref(workspace, session);
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
            terminal: launched.clone(),
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            finished: Mutex::new(finished_tx),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    ui.start_terminal_session(first.clone(), terminal_geometry(20, 80));
    ui.start_terminal_session(second.clone(), terminal_geometry(20, 80));
    assert_eq!(stream.lock().unwrap().attaches, 2);

    let blocked = OperationId::new();
    let queued = OperationId::new();
    let mut pending = std::collections::HashMap::new();
    for operation in [blocked, queued] {
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        pending.insert(operation, target);
        crate::presentation::enqueue_pane_launch(
            &mut ui,
            agent_launch(workspace, session, operation),
        );
    }

    // One worker is admitted and stops inside the launch client. A second
    // drain must not start another request on it.
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(ui.pane_launches.len(), 1);
    assert!(ui.active_pane_launch.is_some());
    assert!(entered.try_recv().is_err());

    // The stopped worker owns nothing the live panes need: both keep polling,
    // accepting input, resizing, and detaching.
    let resizes_before = stream.lock().unwrap().resizes;
    ui.resize_terminals(terminal_geometry(24, 100));
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(ui.send_terminal_bytes(&first, b"a"), Ok(()));
    assert_eq!(ui.send_terminal_bytes(&second, b"b"), Ok(()));
    ui.close_terminal(&second);
    {
        let observed = stream.lock().unwrap();
        assert_eq!(observed.launches, 0);
        assert_eq!(observed.resizes, resizes_before + 2);
        assert_eq!(observed.polls, 2);
        assert_eq!(observed.inputs, [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(observed.detaches, 1);
    }

    // Releasing the barrier completes exactly the admitted pane and frees
    // admission for the one that stayed pending.
    release.send(()).unwrap();
    assert_eq!(
        finished.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    assert!(matches!(
        outcome,
        crate::presentation::PaneLaunchOutcome::Agent { operation, result: Ok(admission) }
            if operation == blocked && admission.terminal.fences(&launched)
    ));
    assert!(ui.active_pane_launch.is_none());
    assert!(!pending.contains_key(&blocked));
    assert!(pending.contains_key(&queued));

    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    assert!(ui.pane_launches.is_empty());
    release.send(()).unwrap();
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    assert!(matches!(
        outcome,
        crate::presentation::PaneLaunchOutcome::Agent { operation, .. } if operation == queued
    ));
    assert!(pending.is_empty());
    // Two requests, two completions, and the stream port was never asked to
    // launch anything.
    assert_eq!(stream.lock().unwrap().launches, 0);
    assert!(ui.pane_completions.try_recv().is_err());
}

/// #522: the operation the controller issued for a pending pane is the one the
/// launch client is asked with — Agent and generic terminal, workspace root and
/// session alike. No adapter mints a second identity whose side effect could be
/// promoted into this pane.
#[test]
fn every_pane_launch_request_carries_its_own_pending_operation() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::new(Mutex::new(StreamCalls::default())),
        Box::new(IdentityRecordingLaunchPort(Arc::clone(&requests))),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();

    let root_agent = OperationId::new();
    let root_goal = OperationId::new();
    let session_agent = OperationId::new();
    let session_terminal = OperationId::new();
    let planned = [
        (
            Target::Root(workspace),
            root_agent,
            "agent",
            crate::presentation::PaneLaunch::Agent {
                operation: root_agent,
                workspace,
                session: None,
                profile: None,
                goal: None,
                resume: false,
            },
        ),
        (
            Target::Root(workspace),
            root_goal,
            "goal",
            crate::presentation::PaneLaunch::Agent {
                operation: root_goal,
                workspace,
                session: None,
                profile: None,
                goal: Some("prepare the PR".to_owned()),
                resume: false,
            },
        ),
        (
            Target::Session(session),
            session_agent,
            "agent",
            agent_launch(workspace, session, session_agent),
        ),
        (
            Target::Session(session),
            session_terminal,
            "terminal",
            crate::presentation::PaneLaunch::Terminal {
                operation: session_terminal,
                workspace,
                session: Some(session),
                arguments: "new".into(),
            },
        ),
    ];
    let expected = planned
        .iter()
        .map(|(target, operation, kind, _)| (*target, *operation, *kind))
        .collect::<Vec<_>>();
    for (target, operation, _, launch) in planned {
        runtime.request_pane(target, operation, PaneKind::Agent);
        pending.insert(operation, target);
        crate::presentation::enqueue_pane_launch(&mut ui, launch);
    }

    // One worker at a time, each drained before the next is admitted.
    for (target, operation, kind) in expected {
        crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        drain_next_completion(&mut ui, &mut runtime, &mut pending);
        let recorded = requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("each admitted launch reaches the client once");
        assert_eq!(recorded.kind, kind);
        assert_eq!(
            recorded.operation, operation,
            "the pending pane's own operation is what the daemon is asked with"
        );
        assert!(
            live_tab_terminals(&runtime, target).contains(&recorded.terminal),
            "the pane promoted the terminal its own operation was answered with"
        );
    }
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(pending.is_empty());
}

#[test]
fn goal_host_action_creates_one_root_pending_pane_and_preserves_the_goal() {
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let mut pending = std::collections::HashMap::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::LaunchAgent(LaunchAgentRequest {
            workspace,
            session: None,
            operation_id: operation,
            profile: None,
            goal: Some("prepare a PR".to_owned()),
        }))
        .unwrap();

    drain_host_actions(
        &receiver,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );

    assert_eq!(pending.get(&operation), Some(&Target::Root(workspace)));
    assert!(matches!(
        runtime
            .panes()
            .pane(Target::Root(workspace))
            .and_then(|pane| pane.tabs().last()),
        Some(PaneTab::Pending(pending)) if pending.operation == operation
    ));
    assert!(matches!(
        ui.pane_launches.as_slice(),
        [crate::presentation::PaneLaunch::Agent {
            operation: actual,
            session: None,
            goal: Some(goal),
            ..
        }] if *actual == operation && goal == "prepare a PR"
    ));
}

/// #522: while a pending operation lives in this process, its completion — even
/// applied out of order — promotes only its own pane, and a completion that
/// arrives after the pending tab is gone revives nothing.
#[test]
fn out_of_order_and_late_completions_never_cross_or_revive_a_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root = Target::Root(workspace);
    let scoped = Target::Session(session);
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::new(Mutex::new(StreamCalls::default())),
        Box::new(IdentityRecordingLaunchPort(Arc::new(
            Mutex::new(Vec::new()),
        ))),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();

    let first = OperationId::new();
    let second = OperationId::new();
    let closed = OperationId::new();
    let first_terminal = scoped_terminal_ref(workspace, None);
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let closed_terminal = scoped_terminal_ref(workspace, Some(session));
    for (target, operation) in [(root, first), (scoped, second), (scoped, closed)] {
        runtime.request_pane(target, operation, PaneKind::Agent);
        pending.insert(operation, target);
    }
    // The third pane is dropped before its daemon answer arrives.
    runtime.fail_pane(scoped, closed, "cancelled".to_owned());

    // The answers arrive in the reverse of the order they were requested.
    for (operation, terminal) in [
        (closed, closed_terminal.clone()),
        (second, second_terminal.clone()),
        (first, first_terminal.clone()),
    ] {
        ui.pane_completion_sender
            .send(crate::presentation::PaneLaunchCompletion {
                launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
                outcome: crate::presentation::PaneLaunchOutcome::Agent {
                    operation,
                    result: Ok(AgentPaneAdmission {
                        terminal,
                        continuation: None,
                        supervisor_run_id: None,
                    }),
                },
            })
            .expect("the workspace still owns its completion receiver");
    }
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    assert_eq!(live_tab_terminals(&runtime, root), vec![first_terminal]);
    assert_eq!(live_tab_terminals(&runtime, scoped), vec![second_terminal]);
    assert!(
        !live_tab_terminals(&runtime, scoped).contains(&closed_terminal),
        "a completion for a closed pending tab never revives it"
    );
    assert!(pending.is_empty());
}

#[test]
fn a_panicking_launch_worker_fails_only_its_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let live = live_terminal_ref(workspace, session);
    let launched = scoped_terminal_ref(workspace, Some(session));
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(PanickingLaunchPort {
            terminal: launched.clone(),
            calls: Arc::clone(&calls),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    ui.start_terminal_session(live.clone(), terminal_geometry(20, 80));
    let dying = OperationId::new();
    let next = OperationId::new();
    let mut pending = std::collections::HashMap::new();
    for operation in [dying, next] {
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        pending.insert(operation, target);
        crate::presentation::enqueue_pane_launch(
            &mut ui,
            agent_launch(workspace, session, operation),
        );
    }

    // The worker unwinds inside the client. Its pane still gets exactly one
    // safe failure, and the shared client is not lost with the thread.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    std::panic::set_hook(hook);
    assert!(matches!(
        outcome,
        crate::presentation::PaneLaunchOutcome::Agent { operation, result: Err(message) }
            if operation == dying && message == crate::presentation::PANE_LAUNCH_WORKER_FAILED
    ));
    assert!(ui.active_pane_launch.is_none());
    assert!(!pending.contains_key(&dying));

    // The live pane never noticed, and the next launch reaches the daemon.
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(ui.send_terminal_bytes(&live, b"c"), Ok(()));
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    assert!(matches!(
        outcome,
        crate::presentation::PaneLaunchOutcome::Agent { operation, result: Ok(admission) }
            if operation == next && admission.terminal.fences(&launched)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(pending.is_empty());
    assert_eq!(stream.lock().unwrap().launches, 0);
}

#[test]
fn a_stale_completion_neither_frees_admission_nor_completes_another_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let stale_terminal = scoped_terminal_ref(workspace, Some(session));
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(UnavailablePaneLaunchPort),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let waiting = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: waiting,
        profile: None,
    });
    let mut pending = std::collections::HashMap::from([(waiting, target)]);
    // A newer worker owns admission.
    ui.active_pane_launch = Some(7);

    // A completion from an older worker, for an operation nobody waits for.
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: 3,
            outcome: crate::presentation::PaneLaunchOutcome::Agent {
                operation: OperationId::new(),
                result: Ok(AgentPaneAdmission {
                    terminal: stale_terminal,
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
        terminal_geometry(20, 80),
    );

    assert_eq!(ui.active_pane_launch, Some(7));
    assert_eq!(pending.get(&waiting), Some(&target));
    assert_eq!(runtime.focused_terminal(), None);
}

#[test]
#[allow(clippy::too_many_lines)] // One sequence fixes both completion kinds and persisted selection.
fn successful_pane_completions_persist_focus_and_select_agent_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let agent_terminal = scoped_terminal_ref(workspace, Some(session));
    let generic_terminal = scoped_terminal_ref(workspace, Some(session));
    let continuation = AgentContinuationRef::new();
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(SuccessfulAgentPort(agent_terminal.clone())),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    assert_eq!(runtime.panes().active(), Some(Target::Session(session)));

    let agent_operation = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: agent_operation,
        profile: None,
    });
    pending.insert(agent_operation, target);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Agent {
                operation: agent_operation,
                result: Ok(AgentPaneAdmission {
                    terminal: agent_terminal.clone(),
                    continuation: Some(continuation),
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
    assert_eq!(runtime.focused_terminal(), Some(agent_terminal.clone()));
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&agent_terminal)
    );

    let terminal_operation = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: terminal_operation,
        arguments: "new".into(),
    });
    pending.insert(terminal_operation, target);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Terminal {
                operation: terminal_operation,
                result: Ok(generic_terminal.clone()),
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
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal));

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Previous))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    assert_eq!(runtime.focused_terminal(), Some(agent_terminal));
    assert!(matches!(
        mutations.lock().unwrap().last(),
        Some(AgentTabIntentMutation::Select {
            session_id: Some(actual),
            continuation: Some(actual_continuation),
        }) if *actual == session && *actual_continuation == continuation
    ));
}

#[test]
fn unavailable_external_terminal_port_returns_a_safe_error() {
    assert_eq!(
        UnavailableExternalTerminalPort.open(Path::new("/tmp/worktree")),
        Err("external terminal launch is unavailable".to_owned())
    );
}

#[test]
fn external_terminal_launch_does_not_require_agent_port() {
    let workspace = WorkspaceId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), WorkspaceState::default(), Vec::new());
    let opened = Arc::new(Mutex::new(Vec::new()));
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort))
        .with_external_terminal(Box::new(RecordingExternalTerminalPort(opened.clone())));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::OpenExternalTerminal(Target::Root(
            workspace,
        )))
        .unwrap();

    drain_host_actions(
        &receiver,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert_eq!(*opened.lock().unwrap(), vec![PathBuf::from("/tmp/demo")]);
}

#[test]
fn render_controller_frame_composites_terminal_launch_failure() {
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{AppEvent, Notice};

    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.apply_event(AppEvent::TerminalLaunchFailed(Notice::new(
        "login shell could not be started",
    )));

    let failure = render_controller_frame(
        20,
        80,
        &runtime,
        "atlas",
        &[],
        None,
        health(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .join("\n");
    assert!(failure.contains("Terminal failed to open"));
    assert!(failure.contains("login shell could not be started"));
}

/// The actual controller path must propagate both a focused-pane change and
/// a later terminal screen revision through `terminal_material_key` and the
/// aggregate `FrameMaterialKey`. Each invalidation owes one viewport/link
/// rebuild and one draw containing the new owned projection.
#[test]
fn terminal_output_change_invalidates_the_joined_material_and_redraws() {
    reset_projection_build_counts();
    let snapshot = snapshot("terminal-cache");
    let terminal = live_terminal_ref(snapshot.workspace_id, snapshot.session_ids[0]);
    let mut keys = vec![Key::Enter, Key::Live(LiveTerminalAction::OpenCloseupModal)];
    keys.extend("terminal open".chars().map(Key::Char));
    keys.push(Key::Enter);
    let mut term = CacheInvalidationTerminal::until_builds(keys, (1, 3));
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(ChangingTerminalPort {
            replay: b"cache-before".to_vec(),
            empty_polls_before_update: 1,
            update: Some(b"\r\ncache-after".to_vec()),
        })),
        launch: Some(Box::new(ImmediateTerminalLaunchPort(terminal))),
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
        Exit::Quit
    );

    let (session_builds, terminal_builds) = projection_build_counts();
    assert_eq!(session_builds, 1, "a terminal change rebuilt session rows");
    assert_eq!(
        terminal_builds, 3,
        "focused pane and screen revision did not each invalidate the terminal key"
    );
    for generation in [2, 3] {
        assert!(
            term.builds_at_draw.contains(&(1, generation)),
            "frame key did not redraw terminal generation {generation}"
        );
    }
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("cache-after")),
        "the redraw did not contain output from the changed terminal screen"
    );
}

#[test]
fn an_exited_terminal_auto_closes_its_pane_and_detaches_through_the_runtime() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let detaches = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 5,
            replay: b"live!".to_vec(),
            poll_error: Some(TerminalError::Exited),
            detaches: Arc::clone(&detaches),
        }),
    );
    assert!(runtime.state().has_live_pane());

    // The per-frame poll sweep observes the exit, drops the tab, and detaches
    // the client subscription — the #1011 behavior lost in the migration.
    close_exited_panes(&mut ui, &mut runtime);

    assert!(runtime.active_pane().tabs().is_empty());
    assert!(!runtime.state().has_live_pane());
    assert_eq!(*detaches.lock().unwrap(), vec![5]);
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "an Agent exit must refresh sidebar and Garden membership immediately"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One scenario drives every epoch transition in order.
fn a_replaced_shared_connection_reattaches_every_pane_before_it_streams_again() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let agent = live_terminal_ref(workspace, session);
    let generic = live_terminal_ref(workspace, session);
    let script = Arc::new(Mutex::new(SharedConnectionScript::default()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_context(
        workspace,
        vec![session],
        Box::new(SharedConnectionPort {
            labels: vec![(agent.clone(), "A"), (generic.clone(), "B")],
            epoch: 1,
            next_subscription: 10,
            attached: Vec::new(),
            ledger: Vec::new(),
            recorded_operations: Vec::new(),
            script: Arc::clone(&script),
            log: Arc::clone(&log),
            writes: Arc::clone(&writes),
        }),
    );
    let geometry = terminal_geometry(20, 80);

    // Both panes attach over one connection and type once.
    ui.start_terminal_session(agent.clone(), geometry);
    ui.start_terminal_session(generic.clone(), geometry);
    assert_eq!(ui.send_terminal_bytes(&agent, b"a"), Ok(()));
    assert_eq!(ui.send_terminal_bytes(&generic, b"b"), Ok(()));

    // 1. Pane A's poll takes a fully received `resync_required`. It resyncs on
    //    the same connection, so B keeps its attachment and its ledger
    //    position, and A continues from the sequence the daemon expects.
    script.lock().unwrap().poll_resync.push("A");
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(ui.send_terminal_bytes(&generic, b"b2"), Ok(()));
    assert_eq!(ui.send_terminal_bytes(&agent, b"a2"), Ok(()));

    // 2. A's viewport resize fails on the resize lane. Neither pane loses its
    //    attachment, so both keep writing on the same subscriptions.
    script.lock().unwrap().resize_failures.push("A");
    ui.resize_terminals(terminal_geometry(24, 100));
    assert_eq!(ui.send_terminal_bytes(&agent, b"a3"), Ok(()));

    // 3. A's input loses the transport before its response completes. The
    //    daemon released B's attachment with that connection too, even though
    //    B never saw a failure.
    script.lock().unwrap().input_transport_eof.push("A");
    assert!(ui.send_terminal_bytes(&agent, b"a4").is_err());

    // B's very next keystroke attaches on the new connection first, and is
    // written exactly once instead of being rejected as unattached.
    assert_eq!(ui.send_terminal_bytes(&generic, b"k"), Ok(()));

    // A recovers through its own reconnect backoff, then both panes stream.
    for _ in 0..200 {
        if ui.poll_all_terminals().is_empty()
            && fenced_traffic(&log.lock().unwrap(), "e2", "A")
                .iter()
                .any(|event| event.contains(" attach "))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // A's lost acknowledgement fenced its producer queue, so reattaching is
    // not enough: the next tick resolves that operation against the daemon's
    // durable record before any later keystroke may reach the PTY (#519).
    assert_eq!(
        ui.send_terminal_bytes(&agent, b"a4-next"),
        Err("terminal input is held in order behind an unresolved input (1 waiting)".to_owned())
    );
    ui.poll_all_terminals();
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|event| event == "e2 input-outcome A")
    );
    // The held keystroke was delivered in order once the fence converged.
    assert_eq!(
        writes.lock().unwrap().last(),
        Some(&("A", b"a4-next".to_vec()))
    );
    assert_eq!(ui.send_terminal_bytes(&agent, b"a5"), Ok(()));

    // Releasing A's pane at the end must not disturb B's attachment.
    ui.close_terminal(&agent);
    assert_eq!(ui.send_terminal_bytes(&generic, b"k2"), Ok(()));
    // Returning to A on the same connection revives its retained coordinator
    // and continues at the daemon ledger cursor instead of restarting at 0.
    ui.start_terminal_session(agent.clone(), geometry);
    assert_eq!(ui.send_terminal_bytes(&agent, b"a6"), Ok(()));

    let log = log.lock().unwrap().clone();
    // No keystroke was ever spent on a released subscription, and no ledger
    // gap opened: the exact cascade this fences off.
    assert!(
        !log.iter()
            .any(|event| event.contains("not-attached") || event.contains("sequence-gap")),
        "{log:#?}"
    );
    // In each epoch, every pane's first attachment-fenced request is its own
    // attach — never a `Resume` or an `Input` on a released subscription.
    for epoch in ["e1", "e2"] {
        for label in ["A", "B"] {
            let traffic = fenced_traffic(&log, epoch, label);
            assert_eq!(
                traffic.first(),
                Some(&format!("{epoch} attach {label}")),
                "{label} in {epoch}: {log:#?}"
            );
        }
    }
    // Exactly one connection replacement happened, and only the failing lane
    // caused it.
    assert_eq!(
        log.iter()
            .filter(|event| event.contains("replaced"))
            .count(),
        1,
        "{log:#?}"
    );
    // B held a subscription from the replaced connection, so its release was
    // local: it was never re-sent on the connection its peers now use, and it
    // came after — and did not revoke — the attach that replaced it.
    assert!(log.contains(&"e2 local-detach B".to_owned()), "{log:#?}");
    // A's same-connection resync detached its own superseded subscription
    // there, where the daemon still held it, and closing A's pane later
    // released its current one the same way.
    assert!(log.contains(&"e1 detach A".to_owned()), "{log:#?}");
    assert!(log.contains(&"e2 detach A".to_owned()), "{log:#?}");
    // Sequences continue across a same-connection resync and restart only on
    // the new connection's fresh ledger.
    for (label, expected) in [
        (
            'A',
            vec![
                "e1 input#0 A",
                "e1 input#1 A",
                "e1 input#2 A",
                // The write whose acknowledgement was lost: the daemon
                // applied it once, which is exactly what the client cannot
                // know until it resolves the operation.
                "e1 input#3 A",
                // The held keystroke, then the one typed after the fence
                // converged. Both on the fresh epoch's restarted sequence.
                "e2 input#0 A",
                "e2 input#1 A",
                // Detach/re-attach on e2 retains the coordinator ledger.
                "e2 input#2 A",
            ],
        ),
        (
            'B',
            vec![
                "e1 input#0 B",
                "e1 input#1 B",
                "e2 input#0 B",
                "e2 input#1 B",
            ],
        ),
    ] {
        assert_eq!(
            log.iter()
                .filter(|event| event.contains(" input#") && event.ends_with(label))
                .cloned()
                .collect::<Vec<_>>(),
            expected,
            "{log:#?}"
        );
    }

    // Every keystroke reached the PTY once, in order, including the first one
    // after the recovery.
    assert_eq!(
        writes.lock().unwrap().clone(),
        vec![
            ("A", b"a".to_vec()),
            ("B", b"b".to_vec()),
            ("B", b"b2".to_vec()),
            ("A", b"a2".to_vec()),
            ("A", b"a3".to_vec()),
            // Applied before the response was lost, and never applied twice.
            ("A", b"a4".to_vec()),
            ("B", b"k".to_vec()),
            // Released from the fence in production order.
            ("A", b"a4-next".to_vec()),
            ("A", b"a5".to_vec()),
            ("B", b"k2".to_vec()),
            ("A", b"a6".to_vec()),
        ]
    );
}

#[test]
fn reconnecting_and_stale_terminal_states_are_projected_into_the_pane_footer() {
    for (error, expected) in [
        (
            TerminalError::Unavailable,
            "daemon unavailable; reconnecting",
        ),
        (TerminalError::Stale, "terminal is no longer available"),
    ] {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (mut ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 6,
                replay: b"retained".to_vec(),
                poll_error: Some(error),
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let mut controls = LiveTerminalControls::default();

        assert!(ui.poll_all_terminals().is_empty());
        let view = controller_terminal_view(&ui, &runtime, &mut controls, 10).unwrap();

        assert_eq!(view.feedback.as_deref(), Some(expected));
        assert_eq!(view.rows[0], "retained");
    }
}

#[test]
fn terminal_reconnect_fake_port_contract() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, _runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 17,
            replay: Vec::new(),
            poll_error: Some(TerminalError::Unavailable),
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );

    assert!(ui.poll_all_terminals().is_empty());
    assert!(!ui.take_terminal_reconnected());
    std::thread::sleep(std::time::Duration::from_millis(110));
    assert!(ui.poll_all_terminals().is_empty());
    assert!(ui.take_terminal_reconnected());
    assert!(!ui.take_terminal_reconnected());
}

#[test]
fn close_tab_live_action_sends_ctrl_d_to_the_focused_agent() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let (ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(WheelRecordingPort {
            terminal,
            replay: Vec::new(),
            inputs: Arc::clone(&inputs),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut ui = ui;
    let mut pending_targets = std::collections::HashMap::new();

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        0,
        0,
    ));

    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert!(ui.closed_generic_terminals.is_empty());
    assert_eq!(*inputs.lock().unwrap(), vec![vec![4]]);
    assert!(runtime.state().notice().is_none());
}

#[test]
fn generic_terminal_ctrl_l_and_ctrl_c_clear_the_shell_and_close_requests_exit() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane_of_kind(
        workspace,
        session,
        terminal.clone(),
        PaneKind::Terminal,
        Box::new(WheelRecordingPort {
            terminal: terminal.clone(),
            replay: b"one\r\ntwo\r\nthree".to_vec(),
            inputs: Arc::clone(&inputs),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Passthrough(vec![12]),
    ));
    assert!(
        !ui.terminal_rows(&terminal, None)
            .expect("focused terminal remains attached")
            .join("\n")
            .contains("one")
    );
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Quit,
    ));

    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        0,
        0,
    ));

    assert_eq!(
        *inputs.lock().unwrap(),
        vec![
            b"\x0c".to_vec(),
            b"\x03\x0c".to_vec(),
            b"\x03exit\r".to_vec()
        ]
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.contains(&terminal));
}

#[test]
fn close_tab_live_action_surfaces_a_safe_delivery_failure() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane_of_kind(
        workspace,
        session,
        terminal.clone(),
        PaneKind::Terminal,
        Box::new(WheelRecordingPort {
            terminal,
            replay: b"retained".to_vec(),
            inputs: Arc::clone(&inputs),
            input_error: true,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        0,
        0,
    ));

    assert!(inputs.lock().unwrap().is_empty());
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(
        runtime.active_pane().error(),
        Some("daemon unavailable; keystroke not delivered")
    );
    assert!(runtime.state().notice().is_none());
}

#[test]
fn focused_pane_feedback_is_visible_in_a_live_terminal_footer() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(WheelRecordingPort {
            terminal,
            replay: b"retained".to_vec(),
            inputs: Arc::new(Mutex::new(Vec::new())),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();

    runtime.surface_focused_pane_feedback("Agent close input was not delivered");
    let view = controller_terminal_view(&ui, &runtime, &mut controls, 10).unwrap();

    assert_eq!(
        view.feedback.as_deref(),
        Some("Agent close input was not delivered")
    );
}

/// `Ctrl-O End` is the way back to live output. A scrolled viewport holds its
/// rows against everything the Agent appends, so the distance to the newest
/// output grows with the conversation and one-line `ScrollDown` alone cannot
/// be the only way back.
#[test]
fn scroll_bottom_returns_a_scrolled_pane_to_the_live_output() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let mut replay = String::new();
    for row in 0..40 {
        use std::fmt::Write as _;
        let _ = writeln!(replay, "row {row}\r");
    }
    let replay = replay.into_bytes();
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 11,
            replay,
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let geometry = terminal_geometry(20, 80);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let mut scroll_key = |key,
                          ui: &mut WorkspaceIoRuntime,
                          runtime: &mut WorkspaceRuntime,
                          controls: &mut LiveTerminalControls,
                          rows_len,
                          scroll| {
        assert!(intercept_live_terminal_control(
            &Key::Live(key),
            ui,
            runtime,
            controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
    };

    let (view, rows_len, scroll) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let live_bottom = view.expect("the focused live tab projects its rows");
    assert_eq!(live_bottom.scroll, 0);

    for _ in 0..5 {
        scroll_key(
            LiveTerminalAction::ScrollUp,
            &mut ui,
            &mut runtime,
            &mut controls,
            rows_len,
            scroll,
        );
    }
    let (scrolled, rows_len, scroll) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let scrolled = scrolled.expect("a scrolled viewport still projects rows");
    assert_eq!(scrolled.scroll, 5);
    assert_ne!(scrolled.rows, live_bottom.rows);

    scroll_key(
        LiveTerminalAction::ScrollBottom,
        &mut ui,
        &mut runtime,
        &mut controls,
        rows_len,
        scroll,
    );
    let (followed, _, _) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    assert_eq!(
        followed.expect("the pane follows live output again"),
        live_bottom
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture audits every pane-only and reducer-owned key.
fn switch_consumes_right_pane_controls_without_mutating_the_dimmed_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let detaches = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 18,
            replay: b"one\ntwo\nthree\nhttps://example.com".to_vec(),
            poll_error: None,
            detaches: Arc::clone(&detaches),
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let rows = vec![
        "one".to_owned(),
        "two".to_owned(),
        "three".to_owned(),
        "https://example.com".to_owned(),
    ];
    let _ = controls.project(rows.clone(), 1);
    controls.scroll_up();
    let before = controls.project(rows.clone(), 1).scroll;
    assert_eq!(before, 1);
    let tabs_before = runtime.active_pane().tabs().to_vec();

    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::OpenCloseupModal));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::ScrollUp),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        4,
        before,
    ));
    let _ = runtime.handle_key(Key::Escape);
    assert!(runtime.wants_live_input());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Switch));
    assert!(!runtime.wants_live_input());
    for key in [
        Key::Live(LiveTerminalAction::ScrollUp),
        Key::Live(LiveTerminalAction::ScrollDown),
        Key::Live(LiveTerminalAction::CloseTab),
        Key::Live(LiveTerminalAction::MoveTabNext),
        Key::Live(LiveTerminalAction::MoveTabPrevious),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Drag,
            column: 41,
            row: 5,
        }),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Up,
            column: 41,
            row: 5,
        }),
    ] {
        assert!(intercept_live_terminal_control(
            &key,
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            4,
            before,
        ));
    }

    for key in [
        Key::Live(LiveTerminalAction::NextTab),
        Key::Passthrough(Vec::new()),
        Key::TerminalCopy {
            fallback: Vec::new(),
        },
        Key::Up,
        Key::Down,
        Key::Left,
        Key::Right,
        Key::Home,
        Key::End,
        Key::Delete,
        Key::LineStart,
        Key::LineEnd,
        Key::SelectLeft,
        Key::SelectRight,
        Key::SelectHome,
        Key::SelectEnd,
        Key::Enter,
        Key::Backspace,
        Key::Tab,
        Key::Escape,
        Key::Quit,
        Key::CtrlQ,
        Key::CtrlD,
        Key::CtrlX,
        Key::Char('x'),
        Key::Click { column: 41, row: 5 },
        Key::Other,
    ] {
        assert!(!intercept_live_terminal_control(
            &key,
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            4,
            before,
        ));
    }
    assert_eq!(controls.project(rows, 1).scroll, before);
    assert!(!controls.has_selection());
    assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
    assert!(detaches.lock().unwrap().is_empty());
    assert!(term.copied.is_empty());
    assert!(browser.opened.is_empty());
}

#[test]
fn root_terminal_pointer_and_wheel_use_the_bottom_drawer_viewport() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(Target::Root(workspace), operation, terminal);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let drawer = crate::presentation::views::root_terminal_drawer::geometry(20, 80);
    let row = u16::try_from(drawer.top + 2).unwrap();

    assert!(!handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 2,
            row,
        },
    ));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 2,
            row,
            notches: 1,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        0,
        0,
    ));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabNext),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        0,
        0,
    ));
}

#[test]
fn close_tab_live_action_cancels_the_focused_pending_launch() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let live = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        live.clone(),
        Box::new(ScriptedAgentPort {
            terminal: live.clone(),
            subscription: 19,
            replay: Vec::new(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let operation = OperationId::new();
    let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
    let _ = runtime.select_tab(crate::usecase::application::controller::TabDirection::Next);
    ui.pane_launches.push(PaneLaunch::Terminal {
        operation: OperationId::new(),
        workspace,
        session: Some(session),
        arguments: "open".to_owned(),
    });
    ui.pane_launches.push(PaneLaunch::Agent {
        operation,
        workspace,
        session: Some(session),
        profile: None,
        goal: None,
        resume: false,
    });
    let mut pending_targets = std::collections::HashMap::from([(operation, target)]);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        0,
        0,
    ));

    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(runtime.focused_terminal(), Some(live));
    assert!(!pending_targets.contains_key(&operation));
    assert!(matches!(
        ui.pane_launches.as_slice(),
        [PaneLaunch::Terminal { .. }]
    ));

    let unqueued = OperationId::new();
    let _ = runtime.request_pane(target, unqueued, PaneKind::Terminal);
    let _ = runtime.select_tab(TabDirection::Next);
    pending_targets.insert(unqueued, target);
    crate::presentation::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending_targets);
    assert!(!pending_targets.contains_key(&unqueued));
    assert!(matches!(
        ui.pane_launches.as_slice(),
        [PaneLaunch::Terminal { .. }]
    ));

    // Closeup still permits dismissing its only pending tab. Switch blocks
    // the same control before it can reach this pane mutation path.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut pending_ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut pending_runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = pending_runtime.handle_key(Key::Down);
    let _ = pending_runtime.handle_key(Key::Enter);
    let operation = OperationId::new();
    let _ = pending_runtime.request_pane(target, operation, PaneKind::Terminal);
    let _ = pending_runtime.select_tab(TabDirection::Next);
    let mut pending_targets = std::collections::HashMap::from([(operation, target)]);
    crate::presentation::close_focused_terminal_pane(
        &mut pending_ui,
        &mut pending_runtime,
        &mut pending_targets,
    );
    assert!(pending_runtime.active_pane().tabs().is_empty());
    assert!(pending_targets.is_empty());
}

#[test]
fn coherent_empty_projection_authoritatively_clears_every_scoped_live_target() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let stale = scoped_terminal_ref(workspace, Some(session));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![LivePane {
                terminal: stale,
                kind: PaneKind::Agent,
            }],
            selected: None,
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    assert_eq!(
        runtime
            .panes()
            .pane(Target::Session(session))
            .unwrap()
            .tabs()
            .len(),
        1
    );

    let empty = crate::presentation::pane_restore_targets(
        workspace,
        &BTreeSet::from([session]),
        AgentTabProjection::default(),
        &[],
        None,
        Vec::new(),
        &BTreeMap::new(),
    );
    assert_eq!(empty.len(), 2);
    assert!(empty.iter().all(|target| target.panes.is_empty()));
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(fence.0, fence.1, empty));
    assert!(
        runtime
            .panes()
            .pane(Target::Session(session))
            .unwrap()
            .tabs()
            .is_empty()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One stream fixture proves simultaneous attachment, polling, and stable geometry.
fn drawers_keep_the_managed_terminal_at_home_geometry_and_moving() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = scoped_terminal_ref(workspace, Some(session));
    let root_agent = scoped_terminal_ref(workspace, None);
    let root_shell = scoped_terminal_ref(workspace, None);
    let initial = b"one\r\ntwo\r\nthree";
    let moved = b"\r\ndim-managed-moved";
    let calls = Arc::new(Mutex::new(StreamCalls {
        scripted_polls: vec![(
            managed.clone(),
            vec![TerminalChunk {
                start_offset: initial.len() as u64,
                end_offset: (initial.len() + moved.len()) as u64,
                data: moved.to_vec(),
            }],
        )],
        ..StreamCalls::default()
    }));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_context(
        workspace,
        vec![session],
        Box::new(RecordingStreamPort(Arc::clone(&calls))),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![
            crate::presentation::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![
                    LivePane {
                        terminal: root_agent.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: root_shell.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(root_agent.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
            crate::presentation::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: managed.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(managed.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));
    let managed_geometry = terminal_geometry(24, 100);
    let managed_background_geometry =
        crate::presentation::managed_background_terminal_geometry(24, 100);
    assert_eq!(managed_background_geometry, managed_geometry);
    let director_geometry = foreground_terminal_geometry(
        24,
        100,
        true,
        false,
        false,
        Some(WorkspaceDrawerFocus::Director),
    );

    ui.sync_foreground_terminal(Some(&managed), managed_geometry);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_drawer_background_attachment(&runtime, &managed);
    ui.sync_visible_terminals(&[
        (root_agent.clone(), director_geometry),
        (managed.clone(), managed_background_geometry),
    ]);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(runtime.focused_terminal(), Some(root_shell.clone()));
    let attachments = crate::presentation::workspace_terminal_attachments(&runtime, 24, 100);
    assert!(attachments.iter().any(|(terminal, geometry)| {
        terminal.fences(&managed) && *geometry == managed_geometry
    }));
    let root_shell_geometry = attachments
        .iter()
        .find_map(|(terminal, geometry)| terminal.fences(&root_shell).then_some(*geometry))
        .expect("the root shell owns the terminal drawer");
    ui.sync_visible_terminals(&attachments);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminalFullHeight));
    assert!(runtime.state().root_terminal_full_height());
    let full_height_attachments =
        crate::presentation::workspace_terminal_attachments(&runtime, 24, 100);
    assert!(full_height_attachments.iter().any(|(terminal, geometry)| {
        terminal.fences(&managed) && *geometry == managed_geometry
    }));
    let full_height_root_geometry = full_height_attachments
        .iter()
        .find_map(|(terminal, geometry)| terminal.fences(&root_shell).then_some(*geometry))
        .expect("the full-height drawer keeps the root shell attached");
    assert_ne!(full_height_root_geometry, root_shell_geometry);
    ui.sync_visible_terminals(&full_height_attachments);
    close_exited_panes(&mut ui, &mut runtime);

    let dimmed_view = ui
        .retained_terminal_view(&managed, 2)
        .expect("the visible managed terminal remains projected");
    assert!(
        strip_ansi(&dimmed_view.rows.join("\n")).contains("dim-managed-moved"),
        "the attached background stream must advance its retained view"
    );
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.attach_geometries,
        [
            (managed.clone(), managed_geometry),
            (root_agent.clone(), director_geometry),
            (root_shell.clone(), root_shell_geometry),
        ]
    );
    assert_eq!(
        calls.resize_geometries,
        [(root_shell.clone(), full_height_root_geometry)],
        "only the terminal inside a resized drawer may change geometry"
    );
    assert!(
        calls
            .poll_terminals
            .iter()
            .any(|terminal| terminal == &root_agent)
    );
    assert!(
        calls
            .poll_terminals
            .iter()
            .any(|terminal| terminal == &root_shell)
    );
    assert!(
        calls
            .poll_terminals
            .iter()
            .any(|terminal| terminal == &managed)
    );
    assert_eq!(
        calls.background_watches.last().cloned(),
        Some(Vec::new()),
        "the attached dimmed terminal must not also enter inventory polling"
    );
    assert_eq!(calls.detaches, 0);
}

#[test]
fn detached_terminal_coordinators_are_bounded_and_evict_the_oldest() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminals = (0..=crate::presentation::DETACHED_TERMINAL_LIMIT)
        .map(|_| scoped_terminal_ref(workspace, Some(session)))
        .collect::<Vec<_>>();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_context(
        workspace,
        vec![session],
        Box::new(UnavailableAgentCommandPort),
    );
    let geometry = terminal_geometry(20, 80);

    // Embedders can lose their stream port before teardown. Closing the
    // retained coordinator still removes and retains it without a detach.
    let without_agent = scoped_terminal_ref(workspace, Some(session));
    let mut embedded = io_runtime(
        WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]),
        Box::new(UnavailableSessionCommandPort),
    );
    embedded.terminals.push(
        crate::usecase::application::terminal_session::TerminalSession::new(
            without_agent.clone(),
            geometry,
        ),
    );
    embedded.close_terminal(&without_agent);
    assert_eq!(embedded.detached_terminals.len(), 1);

    for terminal in &terminals {
        ui.start_terminal_session(terminal.clone(), geometry);
        ui.close_terminal(terminal);
    }

    assert_eq!(
        ui.detached_terminals.len(),
        crate::presentation::DETACHED_TERMINAL_LIMIT
    );
    assert!(
        !ui.detached_terminals
            .iter()
            .any(|retained| retained.terminal().fences(&terminals[0]))
    );
    assert!(
        ui.detached_terminals
            .iter()
            .any(|retained| retained.terminal().fences(terminals.last().unwrap()))
    );
}

#[test]
#[allow(clippy::too_many_lines)] // The stale-cache regression needs both pane kinds and a fresh observation.
fn same_tui_reopen_waits_for_fresh_observation_and_preserves_new_generic_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let agent_terminal = scoped_terminal_ref(workspace, Some(session));
    let generic_terminal = scoped_terminal_ref(workspace, Some(session));
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    // Establish the old empty observation, then admit both panes later in
    // this TUI. Reopen must never rebuild from that obsolete snapshot.
    assert!(
        ui.observe_agent_tabs(
            Vec::new(),
            AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        )
        .unwrap()
        .cas_accepted
    );
    ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: agent_terminal.clone(),
        select: true,
    })
    .unwrap();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: agent_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: generic_terminal.clone(),
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(agent_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    crate::presentation::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.focused_terminal(), Some(agent_terminal.clone()));
    assert!(durable.lock().unwrap().dismissed.is_empty());

    // Seed legacy hidden state to exercise compatibility with an older
    // writer. The current UI itself never creates this state.
    ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss { continuation })
        .unwrap();
    let _ = runtime.close_focused_pane();
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal.clone()));

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::ReopenAgent(ReopenAgentRequest {
            workspace,
            continuation,
        }))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert!(!durable.lock().unwrap().dismissed.contains(&continuation));
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal.clone()));
    assert!(matches!(
        runtime.active_pane().tabs(),
        [PaneTab::Live(LivePane { terminal, kind: PaneKind::Terminal })]
            if terminal.fences(&generic_terminal)
    ));
    assert!(ui.take_agent_observation_request());

    let now = std::time::Duration::from_secs(1);
    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(!retry.complete(
        std::time::Duration::ZERO,
        crate::presentation::RestoreJobOutcome::Applied
    ));
    retry.request_observation(now);
    assert!(retry.begin_if_due(now));
    assert!(!retry.begin_if_due(now));
    let fence = runtime.restore_fence();
    let applied = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![
                TerminalInventoryEntry {
                    terminal: agent_terminal.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: generic_terminal.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                },
            ]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: vec![AgentRuntimeInventoryItem {
                    runtime: AgentRuntimeRef::new(
                        AgentRuntimeId::new(),
                        agent_terminal.clone(),
                        Some(session),
                    )
                    .unwrap(),
                    continuation,
                    state: AgentRuntimeInventoryState::Live,
                    resumed_from: None,
                }],
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        applied.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(!retry.complete(now, applied.outcome));
    let restored = runtime
        .active_pane()
        .tabs()
        .iter()
        .filter_map(|tab| match tab {
            PaneTab::Live(pane) => Some(pane.terminal.clone()),
            PaneTab::Pending(_) | PaneTab::Ready(_) | PaneTab::Interrupted(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(restored, vec![agent_terminal, generic_terminal.clone()]);
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal));
    assert_eq!(
        mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|mutation| matches!(mutation, AgentTabIntentMutation::Observe { .. }))
            .count(),
        2
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture closes, replays, reconnects, and reopens.
fn closing_an_unobserved_live_agent_survives_inventory_replay_and_reconnect() {
    let workspace = WorkspaceId::new();
    let closed_terminal = scoped_terminal_ref(workspace, None);
    let surviving_terminal = scoped_terminal_ref(workspace, None);
    // Nothing is saved yet, so both root conversations are projected from
    // their terminal fence alone (#599) and neither has a continuation.
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::new(),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    // The root target owns the Agent drawer, so it is the active pane only
    // while the drawer is open.
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: vec![
                LivePane {
                    terminal: closed_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: surviving_terminal,
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(closed_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    assert_eq!(ui.agent_continuation_for(&closed_terminal), None);

    crate::presentation::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert_eq!(runtime.focused_terminal(), Some(closed_terminal));
    assert!(durable.lock().unwrap().dismissed.is_empty());
    assert!(durable.lock().unwrap().dismissed_terminals.is_empty());
    assert_eq!(
        runtime.active_pane().error(),
        Some("terminal session is no longer available")
    );
    assert!(runtime.state().notice().is_none());
}

#[test]
fn root_terminal_drawer_retargets_plain_new_to_a_terminal_tab() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));

    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew)),
        Key::Live(LiveTerminalAction::NewRootTerminal)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::NextTab)),
        Key::Live(LiveTerminalAction::NextTab)
    );
}

#[test]
fn root_terminal_drawer_cycles_and_clicks_terminal_only_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        managed.clone(),
        Box::new(WheelRecordingPort {
            terminal: managed,
            replay: Vec::new(),
            inputs: Arc::new(Mutex::new(Vec::new())),
            input_error: false,
        }),
    );
    let first = scoped_terminal_ref(workspace, None);
    let second = scoped_terminal_ref(workspace, None);
    for terminal in [&first, &second] {
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(Target::Root(workspace), operation, terminal.clone());
    }
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let _ = runtime.select_tab_selection(TabSelection::Live(first.clone()));

    assert!(!select_root_terminal_tab(&Key::Other, &mut runtime));
    assert!(select_root_terminal_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut runtime,
    ));
    assert_eq!(runtime.focused_terminal(), Some(second.clone()));
    assert!(select_root_terminal_tab(
        &Key::Live(LiveTerminalAction::PreviousTab),
        &mut runtime,
    ));
    assert_eq!(runtime.focused_terminal(), Some(first));
    assert!(!select_root_terminal_tab(
        &Key::Live(LiveTerminalAction::OpenPullRequests),
        &mut runtime,
    ));

    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();
    let tab_row =
        u16::try_from(crate::presentation::views::root_terminal_drawer::geometry(30, 100).top + 2)
            .unwrap();
    assert!(intercept_live_terminal_control(
        &Key::Click {
            column: 15,
            row: tab_row,
        },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        30,
        100,
        0,
        0,
    ));
    assert_eq!(runtime.focused_terminal(), Some(second));
    assert!(!intercept_live_terminal_control(
        &Key::Click {
            column: 29,
            row: tab_row,
        },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        30,
        100,
        0,
        0,
    ));
}

#[test]
fn a_live_terminal_drag_selects_and_release_copies_to_the_clipboard() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 9,
            replay: b"hello".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let rows_len = ui
        .terminal_rows(&terminal, None)
        .expect("attached live rows")
        .len();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));

    // The right pane starts at column 37 (36-wide sidebar + divider) and its
    // content begins at frame row 5. Drag across "hello" and release.
    let drag = |column| PointerEvent {
        kind: PointerKind::Drag,
        column,
        row: 5,
    };
    assert!(handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 37,
            row: 5,
        },
    ));
    assert!(!controls.has_selection());
    // The next drag report lands at the final "o". The press cell above is
    // still part of the copied range, so this must yield all of "hello".
    handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        drag(41),
    );
    handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Up,
            column: 41,
            row: 5,
        },
    );

    assert_eq!(term.copied, vec!["hello".to_owned()]);
    // The completed selection is retained, so the native copy shortcut can
    // copy it again without needing another mouse release.
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: Vec::new(),
        },
    ));
    assert_eq!(term.copied, vec!["hello".to_owned(), "hello".to_owned()]);
    // Releasing the mouse keeps the range highlighted instead of clearing it,
    // and the projected rows still carry the reverse-video selection.
    assert!(controls.has_selection());
    assert!(!controls.is_dragging());
    let projected = ui
        .terminal_rows(&terminal, controls.selection())
        .expect("selection rows");
    assert!(
        projected.iter().any(|row| row.contains("\u{1b}[7mhello")),
        "selection highlight lost after release: {projected:?}"
    );
    // A drag that copied a selection never also opens a link.
    assert!(browser.opened.is_empty());
}

#[test]
fn retained_terminal_selection_copy_reports_missing_or_empty_selection() {
    let mut term = FakeTerminal::default();
    let mut controls = LiveTerminalControls::default();

    copy_terminal_selection(&mut controls, &mut term);
    assert_eq!(term.copied, Vec::<String>::new());
    assert_eq!(
        controls.project(Vec::new(), 1).feedback.as_deref(),
        Some("no terminal text is selected")
    );

    controls.begin_selection(TerminalSelection::begin(
        vec!["text".to_owned()],
        TerminalPoint { row: 0, column: 4 },
    ));
    copy_terminal_selection(&mut controls, &mut term);
    assert_eq!(term.copied, Vec::<String>::new());
    assert_eq!(
        controls.project(Vec::new(), 1).feedback.as_deref(),
        Some("no terminal text is selected")
    );
}

#[test]
fn a_down_up_click_on_a_terminal_link_opens_it_without_touching_the_pty() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 11,
            replay: b"see https://example.com/x now".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let rows_len = ui
        .terminal_rows(&terminal, None)
        .expect("attached live rows")
        .len();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut controls = LiveTerminalControls::default();
    let mut pending_targets = std::collections::HashMap::new();
    controls.sync_focus(Some(&terminal));

    // A press-release with no drag: the URL starts at content column 4, so
    // frame column 37 + 4 = 41 lands on it. Down must not create the
    // one-cell selection that previously stole the release from link-open.
    assert!(intercept_live_terminal_control(
        &Key::Click { column: 41, row: 5 },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert!(!controls.has_selection());
    assert!(intercept_live_terminal_control(
        &Key::Pointer(PointerEvent {
            kind: PointerKind::Up,
            column: 41,
            row: 5,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert_eq!(browser.opened, vec!["https://example.com/x".to_owned()]);
    // A pointer release is not keyboard input, so nothing was forwarded to the
    // child PTY, and the clipboard was left alone.
    assert!(term.copied.is_empty());

    // A complete click on the leading prose (frame column 37 = content
    // column 0) opens nothing.
    assert!(intercept_live_terminal_control(
        &Key::Click { column: 37, row: 5 },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert!(intercept_live_terminal_control(
        &Key::Pointer(PointerEvent {
            kind: PointerKind::Up,
            column: 37,
            row: 5,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert_eq!(browser.opened.len(), 1);
}

#[test]
fn a_terminal_press_waits_for_drag_and_then_anchors_at_its_start_cell() {
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
            replay: b"hello".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let rows_len = ui
        .terminal_rows(&terminal, None)
        .expect("attached live rows")
        .len();
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));
    // The right pane starts at column 37 and terminal content at row 5. The
    // press anchors the selection at the first "h", before the first drag
    // report reaches the controller.
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    assert!(handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 37,
            row: 5,
        },
    ));
    assert!(!controls.is_dragging());
    assert!(!controls.has_selection());
    handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Drag,
            column: 38,
            row: 5,
        },
    );
    assert!(controls.is_dragging());
    assert_eq!(
        controls.selection().expect("selection started").anchor(),
        TerminalPoint { row: 0, column: 0 }
    );

    // A left-sidebar click remains with sidebar navigation; the terminal
    // interceptor must not consume it.
    assert!(!handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 5,
            row: 2,
        },
    ));
}

#[test]
fn scrolling_a_live_terminal_offsets_its_projected_viewport() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    // Enough output to overflow the viewport so scrolling has headroom.
    let replay: Vec<u8> = (0..40)
        .flat_map(|line| format!("line {line}\r\n").into_bytes())
        .collect();
    let (ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 3,
            replay,
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let viewport_rows = usize::from(terminal_geometry(20, 80).rows);

    // The first projection anchors at the live bottom (scroll 0).
    let live_bottom =
        controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows).expect("live view");
    assert_eq!(live_bottom.scroll, 0);
    assert!(live_bottom.total_rows > live_bottom.rows.len());
    assert_eq!(live_bottom.rows.len(), viewport_rows);
    assert_eq!(
        live_bottom.row_offset + live_bottom.rows.len(),
        live_bottom.total_rows
    );
    controls.scroll_up();
    controls.scroll_up();
    let scrolled =
        controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows).expect("live view");
    assert_eq!(scrolled.scroll, 2);
    assert_eq!(
        scrolled.row_offset + scrolled.rows.len() + scrolled.scroll,
        scrolled.total_rows
    );
}

#[test]
fn open_selection_loads_and_runs_workspace_on_the_same_terminal() {
    let mut term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')]);
    let mut loader = FakeLoader::default();
    assert_eq!(
        run(&mut term, vec![ws("alpha")], Vec::new(), now(), &mut loader,).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(term.frames.len(), 4);
    assert!(term.frames[0].join("\n").contains("Menu"));
    assert!(term.frames[1].join("\n").contains("Open Workspace"));
    assert!(term.frames[2].join("\n").contains("alpha-session"));
}

#[test]
fn agent_command_port_terminal_methods_are_safe_by_default() {
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let mut port = DefaultTerminalPort;
    assert!(
        port.launch(
            OperationId::new(),
            WorkspaceId::new(),
            Some(SessionId::new()),
            None
        )
        .is_err()
    );
    assert_eq!(
        port.resize_terminal(&terminal, Geometry { cols: 80, rows: 24 }),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.attach_terminal(&terminal, Geometry { cols: 80, rows: 24 }),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.poll_terminal(&terminal, 0),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.input_terminal(
            &terminal,
            TerminalSubscription { id: 1, epoch: 1 },
            0,
            OperationId::new(),
            b"x",
        ),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.terminal_input_outcome(&terminal, OperationId::new(), 1),
        Err(TerminalError::Unavailable)
    );
    // Detach is a no-op default and must not panic.
    port.detach_terminal(&terminal, TerminalSubscription { id: 1, epoch: 1 });
    assert_eq!(
        port.launch_terminal(
            WorkspaceId::new(),
            Some(SessionId::new()),
            Geometry { cols: 80, rows: 24 },
            "open",
            OperationId::new(),
        ),
        Err("terminal launch is unavailable".to_owned())
    );
    assert_eq!(port.list_terminals(), Err(TerminalError::Unavailable));
}

#[test]
fn key_to_terminal_bytes_encodes_input_and_forwards_control_chords() {
    assert_eq!(key_to_terminal_bytes(Key::Char('a')), Some(b"a".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Enter), Some(b"\r".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::Backspace),
        Some(b"\x7f".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Tab), Some(b"\t".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Escape), Some(b"\x1b".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Up), Some(b"\x1b[A".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Down), Some(b"\x1b[B".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::PageUp),
        Some(b"\x1b[5~".to_vec())
    );
    assert_eq!(
        key_to_terminal_bytes(Key::PageDown),
        Some(b"\x1b[6~".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Right), Some(b"\x1b[C".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::SelectRight),
        Some(b"\x1b[C".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Left), Some(b"\x1b[D".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::SelectLeft),
        Some(b"\x1b[D".to_vec())
    );
    for key in [Key::Home, Key::LineStart, Key::SelectHome] {
        assert_eq!(key_to_terminal_bytes(key), Some(vec![1]));
    }
    for key in [Key::End, Key::LineEnd, Key::SelectEnd] {
        assert_eq!(key_to_terminal_bytes(key), Some(vec![5]));
    }
    assert_eq!(
        key_to_terminal_bytes(Key::Delete),
        Some(b"\x1b[3~".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Passthrough(Vec::new())), None);
    assert_eq!(
        key_to_terminal_bytes(Key::Passthrough(vec![0xff])),
        Some(vec![0xff])
    );
    assert_eq!(
        key_to_terminal_bytes(Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![0x13],
        }),
        Some(vec![0x13])
    );
    // A program that did not request bracketed paste gets the raw payload;
    // an opted-in Agent gets one marked block. An empty paste sends nothing.
    assert_eq!(
        key_to_terminal_bytes(Key::Paste("a\nb".to_owned())),
        Some(b"a\nb".to_vec())
    );
    assert_eq!(
        key_to_terminal_bytes_for_mode(Key::Paste("a\nb".to_owned()), true),
        Some(b"\x1b[200~a\nb\x1b[201~".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Paste(String::new())), None);
    assert_eq!(key_to_terminal_bytes(Key::Quit), Some(vec![3]));
    assert_eq!(key_to_terminal_bytes(Key::CtrlQ), Some(vec![17]));
    assert_eq!(key_to_terminal_bytes(Key::CtrlD), Some(vec![4]));
    assert_eq!(key_to_terminal_bytes(Key::CtrlX), Some(vec![24]));
    assert_eq!(key_to_terminal_bytes(Key::Help), None);
    assert_eq!(key_to_terminal_bytes(Key::Other), None);
    assert_eq!(
        key_to_terminal_bytes(Key::Live(
            crate::usecase::terminal_input::LiveTerminalAction::NextTab
        )),
        None
    );
}

#[test]
fn terminal_geometry_uses_the_visible_right_pane_width() {
    assert_eq!(terminal_geometry(24, 80), Geometry { cols: 43, rows: 17 });
    // The left sidebar keeps its 36 columns; every remaining terminal
    // column belongs to the right pane even on a wide outer terminal.
    assert_eq!(
        terminal_geometry(34, 153),
        Geometry {
            cols: 116,
            rows: 27
        }
    );
    assert_eq!(
        foreground_terminal_geometry(
            24,
            100,
            true,
            false,
            false,
            Some(WorkspaceDrawerFocus::Director),
        ),
        Geometry { cols: 56, rows: 16 }
    );
    assert_eq!(
        foreground_terminal_geometry(
            24,
            100,
            false,
            true,
            false,
            Some(WorkspaceDrawerFocus::Terminal),
        ),
        Geometry { cols: 96, rows: 7 }
    );
    assert_eq!(
        foreground_terminal_geometry(
            24,
            100,
            true,
            true,
            false,
            Some(WorkspaceDrawerFocus::Terminal),
        ),
        Geometry { cols: 36, rows: 7 }
    );
    assert_eq!(
        foreground_terminal_geometry(24, 100, false, false, false, None),
        terminal_geometry(24, 100)
    );
}

#[test]
fn one_explicit_resume_sends_one_request_and_turns_only_that_tab_live() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resumed = interrupted_history(workspace, Some(session), true);
    let untouched = interrupted_history(workspace, Some(session), true);
    let answer = exact_resume_answer(&resumed);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![resumed.clone(), untouched],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![Ok(answer.clone())],
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    // Nothing has asked the daemon to resume anything yet.
    assert!(requests.lock().unwrap().is_empty());
    crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert_eq!(ui.pane_launches.len(), 1);
    // A repeated activation converges to the in-flight request.
    crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert_eq!(ui.pane_launches.len(), 1);

    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    // Exactly one daemon request, carrying the daemon's own opaque target.
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, *resumed.target.as_ref().unwrap());
    assert_eq!(runtime.focused_terminal(), Some(answer.terminal));
    // The other history tab is unchanged and still unresumed.
    assert_eq!(runtime.active_pane().tabs().len(), 2);
    assert_eq!(
        runtime
            .active_pane()
            .tabs()
            .iter()
            .filter(|tab| matches!(tab, PaneTab::Interrupted(_)))
            .count(),
        1
    );
}

#[test]
fn the_resume_chord_drives_the_selected_history_tab_through_the_live_surface() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let answer = exact_resume_answer(&history);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![Ok(answer.clone())],
            requests: Arc::clone(&requests),
        })),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();

    // `Ctrl-O r` is a pane-only control: it is consumed by the Closeup pane.
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::ResumeTab),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        0,
        0,
    ));
    assert_eq!(ui.pane_launches.len(), 1);

    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending_targets,
        terminal_geometry(20, 80),
    );
    assert_eq!(runtime.focused_terminal(), Some(answer.terminal));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn branch_catalog_worker_does_not_keep_the_resident_catalog_alive() {
    let drops = Arc::new(AtomicUsize::new(0));
    let catalog: Box<dyn crate::presentation::SessionCatalogPort> =
        Box::new(CountedPort(Arc::clone(&drops)));
    let worker = catalog.branch_worker();

    drop(catalog);

    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        worker.branches(Path::new("/tmp/workspace"), None),
        crate::presentation::SessionBranchCatalog::default()
    );
}

#[test]
fn a_terminal_without_an_input_aware_wait_keeps_the_old_splash_pacing() {
    let mut term = SleepingTerminal::default();

    let played = play_startup_splash(&mut term).unwrap();

    let frames = crate::presentation::views::splash::FRAMES;
    assert_eq!(played, frames);
    assert_eq!(term.frames, frames);
    assert_eq!(term.waits.len(), frames);
    assert!(
        term.waits
            .iter()
            .all(|wait| *wait == crate::presentation::views::splash::ANIM_TICK)
    );
}

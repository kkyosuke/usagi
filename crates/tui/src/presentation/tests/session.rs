//! session の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn closeup_live_pr_action_requests_the_active_sessions_prs_without_an_empty_modal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut state =
        crate::usecase::application::controller::AppState::home(workspace, vec![session]);
    let _ = crate::usecase::application::controller::update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    let _ =
        crate::usecase::application::controller::update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(state.overlay(), None);

    let event = app_event_from_key(Key::Live(LiveTerminalAction::OpenPullRequests))
        .expect("live PR action maps to a reducer event");
    assert_eq!(
        crate::usecase::application::controller::update(&mut state, event),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
    assert_eq!(state.pr_request(), Some(target));
}

/// tab strip がまだ無い session では、うさぎの click は空の Closeup への
/// 訪問までで止まり、pane を勝手に選ばない。
#[test]
fn a_rabbit_click_on_a_tabless_session_stops_at_its_closeup() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));

    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let click = GardenClick::Visit {
        workspace,
        session,
        agent: Some(AgentRuntimeId::new()),
    };
    // The Garden overlay owns input before the visit reaches Closeup, so a
    // covered pane strip cannot receive the agent click.
    visit_garden_agent(&mut ui, &mut runtime, click);
    assert_eq!(runtime.focused_terminal(), None);
    let _ = runtime.apply_event(AppEvent::GardenClick(click));
    assert_eq!(runtime.state().overlay(), None);
    visit_garden_agent(&mut ui, &mut runtime, click);
    assert_eq!(runtime.focused_terminal(), None);
}

#[test]
fn goal_pane_launch_rejects_a_managed_session_before_calling_the_port() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let operation = OperationId::new();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let outcome = crate::presentation::run_pane_launch(
        &IdentityRecordingLaunchPort(Arc::clone(&requests)),
        crate::presentation::PaneLaunch::Agent {
            operation,
            workspace,
            session: Some(session),
            profile: None,
            goal: Some("invalid scope".to_owned()),
            resume: false,
        },
        terminal_geometry(20, 80),
    );

    assert!(matches!(
        outcome,
        crate::presentation::PaneLaunchOutcome::Agent {
            operation: actual,
            result: Err(ref reason),
        } if actual == operation && reason.contains("workspace-root scope")
    ));
    assert!(requests.lock().unwrap().is_empty());
}

/// #551 acceptance. The frame loop must be "non-blocking drain → projection
/// → draw → input" and nothing else: neither a wake-up tick nor a resize may
/// reach a daemon lane, and no frame may spawn a session worker. Both used
/// to happen on every `Key::Other`, at 62.5Hz.
#[test]
fn ticks_and_resizes_never_reach_a_daemon_lane_or_spawn_a_session_worker() {
    let decision_wakes = Arc::new(AtomicUsize::new(0));
    let decision_polls = Arc::new(AtomicUsize::new(0));
    let lane_wakes = Arc::new(AtomicUsize::new(0));
    let lane_drains = Arc::new(AtomicUsize::new(0));
    let session_calls = Arc::new(Mutex::new(Vec::new()));

    // Forty wake-ups interleaved with forty resizes — the shape of dragging
    // a window edge while nothing else happens — then a modal open/close and
    // quit, all while both lanes stay silent.
    let mut keys = Vec::new();
    for _ in 0..40 {
        keys.push(Key::Other);
        keys.push(Key::Resize);
    }
    keys.extend([
        Key::Char(':'),
        Key::Char('i'),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(SnapshotSessionPort(Arc::clone(&session_calls)))),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: Some(Box::new(FakeSessionRefreshPort {
            wakes: Arc::clone(&lane_wakes),
            takes: Arc::clone(&lane_drains),
            queued: Arc::default(),
        })),
        decisions: Some(Box::new(CountingDecisionPort {
            wakes: Arc::clone(&decision_wakes),
            polls: Arc::clone(&decision_polls),
        })),
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot("idle"), &mut factory).unwrap(),
        Exit::Quit
    );

    // One seed wake per lane for the whole run — not one per frame.
    assert_eq!(decision_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(lane_wakes.load(Ordering::SeqCst), 1);
    // The command port, and therefore `std::thread::spawn`, is untouched:
    // the tick no longer runs `SessionCommand::List`.
    assert!(
        session_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    // What the loop does do every frame is drain. Since #554 the redraw is
    // gated on the frame's material, so a tick that changes nothing draws
    // nothing — the per-iteration invariant lives in the drain counts, not
    // in the frame count.
    assert!(decision_polls.load(Ordering::SeqCst) >= 80);
    assert!(lane_drains.load(Ordering::SeqCst) >= 80);
    // Draw, modal, and quit all completed with both lanes never answering.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Overview"))
    );
}

/// The session revision must cross both cache gates in the real composition
/// loop: first rebuild the owned row/path projection, then rebuild and draw
/// the frame that contains it.
#[test]
fn daemon_session_change_invalidates_the_joined_material_and_redraws() {
    reset_projection_build_counts();
    let snapshot = snapshot("session-cache");
    let original = snapshot.session_ids[0];
    let added = SessionId::new();
    let mut added_record = snapshot.state.sessions[0].clone();
    added_record.name = "cache-added".to_owned();
    added_record.root = PathBuf::from("/tmp/session-cache/cache-added");
    let update = SessionCommandResult {
        message: "daemon snapshot changed".to_owned(),
        sessions: Some(vec![snapshot.state.sessions[0].clone(), added_record]),
        session_ids: Some(vec![original, added]),
        agent_resumes: None,
        session_lifecycles: None,
        session_roles: None,
        revision: Some(1),
    };
    let mut term = CacheInvalidationTerminal::scripted([
        Key::Other,
        Key::Other,
        Key::Other,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: Some(Box::new(ScheduledSessionRefreshPort {
            publish_on_take: 3,
            takes: 0,
            update: Some(update),
        })),
        decisions: None,
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
        Exit::Quit
    );

    let (session_builds, terminal_builds) = projection_build_counts();
    assert_eq!(session_builds, 2, "the changed session key did not rebuild");
    assert_eq!(terminal_builds, 1, "a session change rebuilt the terminal");
    assert!(
        term.builds_at_draw.contains(&(2, 1)),
        "the frame key did not redraw after the session material rebuild"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("cache-added")),
        "the redrawn frame did not contain the changed session projection"
    );
}

#[test]
fn malformed_session_identity_refreshes_clear_rows_ids_and_agent_targets() {
    let workspace = WorkspaceId::new();
    let first = SessionId::new();
    let second = SessionId::new();
    let mut records = state("demo").sessions;
    records.push(SessionRecord {
        name: "second".to_owned(),
        root: "/tmp/demo/.usagi/sessions/second".into(),
        ..records[0].clone()
    });

    for invalid_ids in [None, Some(vec![first]), Some(vec![first, first])] {
        let view = WorkspaceView::with_runtime_ids(
            ws("demo"),
            WorkspaceState {
                sessions: records.clone(),
                ..WorkspaceState::default()
            },
            vec![first, second],
        );
        let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![first, second],
                Box::new(UnavailableAgentCommandPort),
            );

        crate::presentation::apply_session_projection(
            &mut ui,
            Some(records.clone()),
            invalid_ids,
            None,
            None,
            None,
        );

        assert!(ui.workspace.sessions().is_empty());
        assert!(ui.workspace.session_ids().is_empty());
        assert!(ui.agent.as_ref().unwrap().sessions.is_empty());
    }
}

#[test]
fn controller_loop_opens_the_create_form_from_the_new_session_row() {
    // An empty workspace shows only root and `+ new session`, so one Down
    // reaches the create entry deterministically.
    let snapshot = snapshot_with_generated_runtime_ids(
        ws("empty"),
        WorkspaceState {
            sessions: Vec::new(),
            root_notes: Scratchpad::default(),
            updated_at: now(),
        },
    );
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: snapshot.workspace_id,
        session_id: None,
        worktree_id: WorktreeId::new(),
    };
    // Down → + new session, Enter opens the create form, type a name, Esc
    // closes it, then Ctrl-Q + y detaches.
    let keys = [
        Key::Down,
        Key::Enter,
        Key::Char('a'),
        Key::Char('p'),
        Key::Char('i'),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    let result = run_workspace_controller(
        &mut term,
        snapshot,
        Box::new(UnavailableSessionCommandPort),
        Box::new(SuccessfulAgentPort(terminal.clone())),
        launch_port(Box::new(SuccessfulAgentPort(terminal))),
        Box::new(UnavailableDecisionCommandPort),
        Box::new(UnavailableEnvironmentStore),
        Box::new(NoDesktopNotifications),
        Box::new(NoMetrics),
        Box::new(UnavailablePrSnapshotPort),
        Box::new(UnavailableBrowserOpener),
    );

    assert!(matches!(result, Ok(Exit::Quit)));
    // The inline `+ new session` row rendered the typed name, confirming the
    // create-entry seam works through the controller loop. It is inline in the
    // sidebar, not a centered modal, so the old "New session" modal title never
    // appears.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("api"))
    );
    assert!(
        term.frames
            .iter()
            .all(|frame| !frame.join("\n").contains("New session"))
    );
}

#[test]
fn controller_loop_dispatches_each_ctrl_a_representation_once_to_the_session_port() {
    struct SignallingSessionPort {
        calls: Arc<AtomicUsize>,
        create_call: std::sync::mpsc::Sender<String>,
    }

    impl SessionCommandPort for SignallingSessionPort {
        fn execute(
            &self,
            _: &Workspace,
            _: Option<&SessionRecord>,
            command: SessionCommand,
        ) -> Result<SessionCommandResult, String> {
            let SessionCommand::Create { name, .. } = command else {
                return Err("unexpected session command".to_owned());
            };
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.create_call
                .send(name)
                .map_err(|error| error.to_string())?;
            Ok(SessionCommandResult::message("daemon accepted"))
        }
    }

    // The composition adapter normalizes a modified Ctrl+A to LineStart,
    // preserves a raw control byte as U+0001, and carries Home as Home. All
    // three must enter the same controller form and lifecycle dispatch path.
    for create_key in [Key::LineStart, Key::Char('\u{1}'), Key::Home] {
        let snapshot = snapshot_with_generated_runtime_ids(
            ws("empty"),
            WorkspaceState {
                sessions: Vec::new(),
                root_notes: Scratchpad::default(),
                updated_at: now(),
            },
        );
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: snapshot.workspace_id,
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let (create_call, observed_create) = std::sync::mpsc::channel();
        let keys = [
            create_key.clone(),
            Key::Char('a'),
            Key::Char('p'),
            Key::Char('i'),
            Key::Enter,
            Key::CtrlQ,
            Key::Char('y'),
        ];
        let mut term = FakeTerminal::with_keys_waiting_for_create(&keys, observed_create);

        let result = run_workspace_controller(
            &mut term,
            snapshot,
            Box::new(SignallingSessionPort {
                calls: calls.clone(),
                create_call,
            }),
            Box::new(SuccessfulAgentPort(terminal.clone())),
            launch_port(Box::new(SuccessfulAgentPort(terminal))),
            Box::new(UnavailableDecisionCommandPort),
            Box::new(UnavailableEnvironmentStore),
            Box::new(NoDesktopNotifications),
            Box::new(NoMetrics),
            Box::new(UnavailablePrSnapshotPort),
            Box::new(UnavailableBrowserOpener),
        );

        assert!(matches!(result, Ok(Exit::Quit)), "{create_key:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{create_key:?}");
        assert_eq!(term.observed_creates, ["api"], "{create_key:?}");
    }
}

#[test]
fn drain_session_completions_refluxes_create_failure_with_its_token() {
    let snapshot = snapshot("demo");
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let token = PendingToken::from_raw(41);

    // A create worker returned a display-safe daemon rejection (e.g. a name the
    // daemon refuses). The legacy path used to drop this on the floor; it must
    // now reflux as a controller notice so the user sees the failure.
    let (backend_completions, backend_receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let result = Err("daemon refused the session".to_owned());
    let completion = crate::presentation::SessionBackendCompletion::Create {
        token,
        before: Vec::new(),
        completions: backend_completions,
    };
    crate::presentation::emit_session_command_result(&result, &completion);
    ui.active_session_command = Some(1);
    ui.session_completion_sender
        .send(crate::presentation::SessionCommandCompletion {
            command_id: 1,
            result,
            completion,
        })
        .unwrap();

    crate::presentation::drain_session_completions(&mut ui);
    assert!(matches!(
        backend_receiver.recv().unwrap(),
        AppEvent::OperationResult(result)
            if result.token == token
                && !result.succeeded
                && result.created.is_none()
                && result.notice.as_ref().is_some_and(|notice| notice.message == "daemon refused the session")
    ));
}

#[test]
fn session_commands_reject_the_second_request_as_busy() {
    let snapshot = snapshot("demo");
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let (first_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let (second_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();

    assert!(crate::presentation::begin_session_command(
        &mut ui,
        SessionCommand::List,
        crate::presentation::SessionBackendCompletion::Remove {
            session: SessionId::new(),
            before: Vec::new(),
            completions: first_completions,
        },
    ));
    assert!(!crate::presentation::begin_session_command(
        &mut ui,
        SessionCommand::List,
        crate::presentation::SessionBackendCompletion::Remove {
            session: SessionId::new(),
            before: Vec::new(),
            completions: second_completions,
        },
    ));
}

#[test]
fn stale_session_completion_does_not_replace_a_newer_snapshot() {
    let snapshot = snapshot("demo");
    let original = snapshot.session_ids[0];
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let (newer_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let (older_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let newer = SessionId::new();
    let mut newer_record = ui.workspace.sessions()[0].clone();
    newer_record.name = "newer".to_owned();

    ui.active_session_command = Some(2);
    ui.session_completion_sender
        .send(crate::presentation::SessionCommandCompletion {
            command_id: 2,
            result: Ok(SessionCommandResult {
                message: "newer".to_owned(),
                sessions: Some(vec![newer_record]),
                session_ids: Some(vec![newer]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(2),
            }),
            completion: crate::presentation::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: vec![original],
                completions: newer_completions,
            },
        })
        .unwrap();
    crate::presentation::drain_session_completions(&mut ui);

    ui.active_session_command = Some(1);
    ui.session_completion_sender
        .send(crate::presentation::SessionCommandCompletion {
            command_id: 1,
            result: Ok(SessionCommandResult {
                message: "older".to_owned(),
                sessions: Some(ui.workspace.sessions().to_vec()),
                session_ids: Some(vec![original]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(1),
            }),
            completion: crate::presentation::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: vec![newer],
                completions: older_completions,
            },
        })
        .unwrap();

    crate::presentation::drain_session_completions(&mut ui);
    assert_eq!(ui.workspace.session_ids(), &[newer]);
    assert_eq!(ui.workspace.sessions()[0].name, "newer");
}

#[test]
fn drain_session_completions_refluxes_create_success_with_created_identity() {
    let snapshot = snapshot("demo");
    let existing = snapshot.session_ids[0];
    let created = SessionId::new();
    let mut records = snapshot.state.sessions.clone();
    let mut new_record = records[0].clone();
    new_record.name = "created".to_owned();
    records.push(new_record);
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let token = PendingToken::from_raw(42);
    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let result = Ok(SessionCommandResult {
        message: "created".to_owned(),
        sessions: Some(records),
        session_ids: Some(vec![existing, created]),
        agent_resumes: None,
        session_lifecycles: None,
        session_roles: None,
        revision: None,
    });
    let completion = crate::presentation::SessionBackendCompletion::Create {
        token,
        before: vec![existing],
        completions,
    };
    crate::presentation::emit_session_command_result(&result, &completion);
    ui.active_session_command = Some(1);

    ui.session_completion_sender
        .send(crate::presentation::SessionCommandCompletion {
            command_id: 1,
            result,
            completion,
        })
        .unwrap();
    crate::presentation::drain_session_completions(&mut ui);

    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::OperationResult(result)
            if result.token == token && result.succeeded && result.created == Some(created)
    ));
}

#[test]
fn session_snapshot_completion_preserves_fallback_and_reports_failure_once() {
    let existing = SessionId::new();
    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = crate::presentation::SessionBackendCompletion::Remove {
        session: SessionId::new(),
        before: vec![existing],
        completions,
    };
    crate::presentation::emit_session_command_result(
        &Ok(SessionCommandResult::message("legacy snapshot")),
        &completion,
    );
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Sessions(sessions)) if sessions == [existing]
    ));
    assert!(receiver.try_recv().is_err());

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = crate::presentation::SessionBackendCompletion::Remove {
        session: SessionId::new(),
        before: vec![existing],
        completions,
    };
    crate::presentation::emit_session_command_result(
        &Err("daemon unavailable".to_owned()),
        &completion,
    );
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(notice)) if notice.message == "daemon unavailable"
    ));
    assert!(receiver.try_recv().is_err());

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = crate::presentation::SessionBackendCompletion::Sleep {
        before: vec![existing],
        completions,
    };
    crate::presentation::emit_session_command_result(
        &Ok(SessionCommandResult::message("slept")),
        &completion,
    );
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Sessions(sessions)) if sessions == [existing]
    ));

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = crate::presentation::SessionBackendCompletion::Sleep {
        before: vec![existing],
        completions,
    };
    crate::presentation::emit_session_command_result(&Err("sleep refused".to_owned()), &completion);
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(notice)) if notice.message == "sleep refused"
    ));
}

#[test]
fn session_worker_panic_completes_and_returns_the_port() {
    let snapshot = snapshot("demo");
    let workspace = snapshot.workspace_id;
    let session = snapshot.session_ids[0];
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(
        view,
        Box::new(PanicOnceSessionPort {
            existing: session,
            created: SessionId::new(),
            panics: AtomicBool::new(true),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (mut host, actions) = ControllerHost::channel();
    let failed = enqueue_session_request(
        &mut host,
        ConcurrentSessionRequest::Create(1),
        workspace,
        session,
    );
    drain_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(matches!(
        failed
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::OperationResult(result)
            if !result.succeeded
                && result.notice.as_ref().is_some_and(|notice| notice.message == "session command worker failed")
    ));
    for _ in 0..100 {
        drain_session_completions(&mut ui);
        if ui.active_session_command.is_none() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(ui.active_session_command.is_none());

    let recovered = enqueue_session_request(
        &mut host,
        ConcurrentSessionRequest::Create(2),
        workspace,
        session,
    );
    drain_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(matches!(
        recovered
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::OperationResult(result) if result.succeeded
    ));
}

#[test]
fn closed_session_host_channel_completes_each_effect_once() {
    use crate::usecase::application::daemon_backend::SessionLifecyclePort as _;

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let (mut host, actions) = ControllerHost::channel();
    drop(actions);

    for request in [
        ConcurrentSessionRequest::Create(1),
        ConcurrentSessionRequest::Remove,
        ConcurrentSessionRequest::Sleep,
    ] {
        let completion = enqueue_session_request(&mut host, request, workspace, session);
        assert!(matches!(
            completion
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            AppEvent::OperationResult(_) | AppEvent::Backend(BackendEvent::Notice(_))
        ));
        assert!(completion.try_recv().is_err());
    }

    let (completions, completion) =
        crate::usecase::application::daemon_backend::Completions::channel();
    host.refresh(workspace, completions);
    assert!(matches!(
        completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::Backend(BackendEvent::Notice(_))
    ));
    assert!(completion.try_recv().is_err());
}

#[test]
fn out_of_order_session_completion_cannot_release_the_active_port() {
    let snapshot = snapshot("demo");
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    ui.active_session_command = Some(2);
    let result = Ok(SessionCommandResult::message("done"));
    let (completions, _) = crate::usecase::application::daemon_backend::Completions::channel();

    ui.session_completion_sender
        .send(crate::presentation::SessionCommandCompletion {
            command_id: 1,
            result: result.clone(),
            completion: crate::presentation::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: Vec::new(),
                completions,
            },
        })
        .unwrap();
    drain_session_completions(&mut ui);
    assert_eq!(ui.active_session_command, Some(2));

    let (completions, _) = crate::usecase::application::daemon_backend::Completions::channel();
    ui.session_completion_sender
        .send(crate::presentation::SessionCommandCompletion {
            command_id: 2,
            result,
            completion: crate::presentation::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: Vec::new(),
                completions,
            },
        })
        .unwrap();
    drain_session_completions(&mut ui);
    assert_eq!(ui.active_session_command, None);
}

#[test]
fn session_snapshot_adapter_preserves_reconciliation_boundary_for_pointer_state() {
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{HomeMode, Route};

    let snapshot = snapshot("demo");
    let workspace_id = snapshot.workspace_id;
    let session = snapshot.session_ids[0];
    let records = snapshot.state.sessions.clone();
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace_id, vec![session]);
    let _ = runtime.apply_event(AppEvent::Resize {
        width: 100,
        height: 30,
    });
    let _ = runtime.apply_event(sidebar_pointer_event(
        5,
        2,
        std::time::Duration::from_millis(1_000),
    ));

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let result = Ok(SessionCommandResult {
        message: "same snapshot".to_owned(),
        sessions: Some(records),
        session_ids: Some(vec![session]),
        agent_resumes: None,
        session_lifecycles: None,
        session_roles: None,
        revision: None,
    });
    let completion = crate::presentation::SessionBackendCompletion::Remove {
        session: SessionId::new(),
        before: vec![session],
        completions,
    };
    crate::presentation::emit_session_command_result(&result, &completion);
    ui.active_session_command = Some(1);
    ui.session_completion_sender
        .send(crate::presentation::SessionCommandCompletion {
            command_id: 1,
            result,
            completion,
        })
        .unwrap();
    crate::presentation::drain_session_completions(&mut ui);
    let _ = runtime.apply_event(receiver.recv().unwrap());
    assert_eq!(runtime.state().sessions(), &[session]);
    let _ = runtime.apply_event(sidebar_pointer_event(
        5,
        2,
        std::time::Duration::from_millis(1_100),
    ));

    let _ = workspace_id;
    assert_eq!(runtime.state().active(), Some(session));
    assert!(matches!(
        runtime.state().route(),
        Route::Home(HomeMode::Switch)
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // Lifecycle cleanup, durable state, and retry admission share one fixture.
fn session_membership_change_requests_one_observation_and_cleans_owned_intent() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let removed_session = SessionId::new();
    let root_open = AgentContinuationRef::new();
    let root_dismissed = AgentContinuationRef::new();
    let removed_selected = AgentContinuationRef::new();
    let removed_dismissed = AgentContinuationRef::new();
    let root_open_terminal = scoped_terminal_ref(workspace, Some(session));
    let root_dismissed_terminal = scoped_terminal_ref(workspace, Some(session));
    let removed_selected_terminal = scoped_terminal_ref(workspace, Some(removed_session));
    let removed_dismissed_terminal = scoped_terminal_ref(workspace, Some(removed_session));
    let mut initial = AgentTabIntent::empty(workspace);
    for (session_id, continuation, terminal, select) in [
        (Some(session), root_open, root_open_terminal.clone(), true),
        (
            Some(session),
            root_dismissed,
            root_dismissed_terminal.clone(),
            false,
        ),
        (
            Some(removed_session),
            removed_selected,
            removed_selected_terminal.clone(),
            true,
        ),
        (
            Some(removed_session),
            removed_dismissed,
            removed_dismissed_terminal.clone(),
            false,
        ),
    ] {
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id,
            continuation,
            terminal,
            select,
        });
    }
    initial.apply(AgentTabIntentMutation::Dismiss {
        continuation: root_dismissed,
    });
    initial.apply(AgentTabIntentMutation::Dismiss {
        continuation: removed_dismissed,
    });
    initial.revision = 9;
    initial.validate(workspace).unwrap();
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session, removed_session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut retry = crate::presentation::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let initial_fence = runtime.restore_fence();
    let initial_pairs = [
        (root_open_terminal.clone(), root_open),
        (root_dismissed_terminal, root_dismissed),
        (removed_selected_terminal, removed_selected),
        (removed_dismissed_terminal, removed_dismissed),
    ];
    let initial_restore = crate::presentation::apply_restore_completion(
        crate::presentation::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: initial_fence.0,
            dispatched_registry_revision: initial_fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session, removed_session]),
            terminals: Ok(initial_pairs
                .iter()
                .map(|(terminal, _)| TerminalInventoryEntry {
                    terminal: terminal.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                })
                .collect()),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: initial_pairs
                    .iter()
                    .map(|(terminal, continuation)| AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            terminal.clone(),
                            terminal.session_id,
                        )
                        .unwrap(),
                        continuation: *continuation,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    })
                    .collect(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session, removed_session]),
    );
    assert_eq!(
        initial_restore.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(!retry.complete(std::time::Duration::ZERO, initial_restore.outcome));
    assert_eq!(mutations.lock().unwrap().len(), 1);
    assert!(!ui.take_agent_observation_request());

    ui.set_allowed_agent_sessions(BTreeSet::from([session]));
    assert!(ui.take_agent_observation_request());
    ui.set_allowed_agent_sessions(BTreeSet::from([session]));
    assert!(!ui.take_agent_observation_request());
    let now = std::time::Duration::from_secs(1);
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
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: root_open_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            }]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: vec![AgentRuntimeInventoryItem {
                    runtime: AgentRuntimeRef::new(
                        AgentRuntimeId::new(),
                        root_open_terminal.clone(),
                        Some(session),
                    )
                    .unwrap(),
                    continuation: root_open,
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
    let mutations = mutations.lock().unwrap();
    assert_eq!(mutations.len(), 2);
    assert!(matches!(
        mutations.as_slice(),
        [
            AgentTabIntentMutation::Observe {
                allowed_sessions: initial_allowed,
                ..
            },
            AgentTabIntentMutation::Observe {
                allowed_sessions: removed_allowed,
                ..
            }
        ] if *initial_allowed == BTreeSet::from([session, removed_session])
            && *removed_allowed == BTreeSet::from([session])
    ));
    drop(mutations);
    let durable = durable.lock().unwrap();
    durable.validate(workspace).unwrap();
    assert!(
        durable
            .targets
            .iter()
            .all(|target| target.session_id != Some(removed_session))
    );
    assert_eq!(durable.dismissed, BTreeSet::from([root_dismissed]));
    assert!(
        durable.targets[0]
            .tabs
            .iter()
            .any(|slot| slot.continuation == root_open)
    );
    assert!(!durable.dismissed.contains(&removed_dismissed));
    assert_eq!(runtime.focused_terminal(), Some(root_open_terminal));
    assert!(!retry.begin_if_due(now + std::time::Duration::from_secs(60)));
}

#[test]
fn concurrent_drawers_keep_root_surfaces_and_selected_session_agent_visible() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = scoped_terminal_ref(workspace, Some(session));
    let root_agent = scoped_terminal_ref(workspace, None);
    let root_terminal = scoped_terminal_ref(workspace, None);
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![
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
            crate::presentation::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![
                    LivePane {
                        terminal: root_agent.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: root_terminal.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(root_agent.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert!(runtime.state().director_drawer_open());
    assert!(runtime.state().root_terminal_drawer_open());
    assert_eq!(runtime.focused_terminal(), Some(root_terminal.clone()));
    let visible = crate::presentation::workspace_terminal_attachments(&runtime, 30, 160);
    assert_eq!(
        visible
            .iter()
            .map(|(terminal, _)| terminal)
            .collect::<Vec<_>>(),
        [&root_terminal, &managed, &root_agent]
    );
    assert_eq!(
        visible[1].1,
        terminal_geometry(30, 160),
        "drawers must not shrink the background workspace geometry"
    );
    assert_eq!(
        visible[0].1,
        Geometry { cols: 60, rows: 10 },
        "the root Shell PTY must fit the band left of Director"
    );
    let fully_occluded = crate::presentation::workspace_terminal_attachments(&runtime, 8, 160);
    assert!(
        fully_occluded
            .iter()
            .any(|(terminal, _)| terminal.fences(&managed)),
        "a full-height root overlay must retain its background Agent"
    );
    assert_eq!(
        crate::presentation::views::workspace::root_terminal_available_width(30, 160, true),
        64
    );
    assert_eq!(
        crate::presentation::views::workspace::root_terminal_available_width(30, 79, true),
        79
    );

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(root_agent.clone()));
    let visible = crate::presentation::workspace_terminal_attachments(&runtime, 30, 160)
        .into_iter()
        .map(|(terminal, _)| terminal)
        .collect::<Vec<_>>();
    assert_eq!(visible, [root_agent, managed, root_terminal]);
}

#[test]
fn workspace_switch_keeps_the_project_bar_and_cached_sessions_while_content_loads() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();

    let pending = cached_workspace_switch_frame(
        &deck,
        &beta.workspace.path,
        24,
        80,
        0,
        "Opening workspace…",
        false,
    )
    .expect("an already-open project has a local transition projection")
    .join("\n");
    let frame = cached_workspace_switch_frame(
        &deck,
        &beta.workspace.path,
        24,
        80,
        2,
        "Opening workspace…",
        true,
    )
    .expect("an already-open project has a local transition projection")
    .join("\n");

    assert!(frame.contains("1 alpha"));
    assert!(frame.contains("2 beta"));
    assert!(frame.contains("beta-session"));
    assert!(!frame.contains("alpha-session"));
    assert!(frame.contains("Opening workspace…"));
    assert!(pending.contains("beta-session"));
    assert!(!pending.contains("Opening workspace…"));
}

#[test]
fn step_config_routes_workspace_session_setup_editor_input_and_cancel() {
    let mut settings = RecordingSettingsPort::default();
    let mut config =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut config, Key::Down, &mut settings);
    }
    let _ = step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_editing_setup_commands());
    for key in [
        Key::Char('x'),
        Key::Paste("y".to_owned()),
        Key::Backspace,
        Key::Left,
        Key::Delete,
        Key::Paste("one\r\ntwo".to_owned()),
        Key::Up,
        Key::Down,
        Key::Home,
        Key::Right,
        Key::End,
        Key::LineStart,
        Key::LineEnd,
        Key::Enter,
        Key::Paste("three".to_owned()),
        Key::Tab,
        Key::Other,
        Key::Tab,
        Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![19],
        },
    ] {
        let _ = step_config(&mut config, key, &mut settings);
    }
    let _ = step_config(&mut config, Key::Escape, &mut settings);
    assert!(!config.is_editing_setup_commands());
    assert_eq!(settings.setup_saves, 0);

    let _ = step_config(&mut config, Key::Enter, &mut settings);
    let _ = step_config(
        &mut config,
        Key::Paste("cargo fetch\ncargo test".to_owned()),
        &mut settings,
    );
    let _ = step_config(&mut config, Key::Tab, &mut settings);
    let _ = step_config(&mut config, Key::Enter, &mut settings);
    assert!(!config.is_editing_setup_commands());
    assert_eq!(settings.setup_commands, ["cargo fetch", "cargo test"]);
}

#[test]
fn workspace_config_dispatches_background_session_setup_save() {
    let base = vec!["home".to_owned(); 28];
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut term = FakeTerminal::with_keys(&[
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::Paste("cargo fetch\ncargo test".to_owned()),
        Key::Tab,
        Key::Enter,
        Key::Escape,
    ]);

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    assert_eq!(settings.setup_saves, 1);
    assert_eq!(settings.setup_commands, ["cargo fetch", "cargo test"]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Session setup"))
    );
}

#[test]
fn safe_session_error_collapses_daemon_output_to_one_safe_line() {
    // 空メッセージは汎用の一行へフォールバックする。
    assert_eq!(safe_session_error(""), "could not create the session");
    assert_eq!(
        safe_session_error("   \n  "),
        "could not create the session"
    );
    // 複数行の出力は先頭行だけを trim して残す（後続の内部詳細を漏らさない）。
    let multi = "session name already exists\n  at daemon::lifecycle::create (secret path)";
    assert_eq!(safe_session_error(multi), "session name already exists");
    // 長い先頭行は切り詰めず全文を保つ（dialog が幅に合わせて折り返して全文表示する）。
    let notice = safe_session_error(&"x".repeat(200));
    assert_eq!(notice.chars().count(), 200);
    assert!(!notice.contains('…'));
}

/// Welcome→Open で開いた workspace が、hard-code の `UnavailableSessionCommandPort`
/// ではなく注入 factory から port を取り出すこと（＝本 fix）を固定する。factory が
/// production では daemon port を返すため、これで全経路が実 port を通ることを担保する。
#[test]
fn open_workspace_pulls_the_session_command_port_from_the_factory() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let created = Arc::new(Mutex::new(0usize));
    let mut factory = SnapshotSessionPortFactory {
        calls: calls.clone(),
        created: created.clone(),
    };
    let keys = [Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;

    assert_eq!(
        run_with_settings(
            &mut term,
            vec![ws("alpha")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(*created.lock().unwrap(), 1);
}

/// Welcome の Recent 経由で開いた workspace も同じ factory から port を取り出す。
#[test]
fn recent_workspace_pulls_the_session_command_port_from_the_factory() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let created = Arc::new(Mutex::new(0usize));
    let mut factory = SnapshotSessionPortFactory {
        calls: calls.clone(),
        created: created.clone(),
    };
    let keys = [Key::Char('1'), Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            vec![recent("home")],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/home")]);
    assert_eq!(*created.lock().unwrap(), 1);
}

#[test]
fn session_command_result_message_carries_no_projection() {
    let result = SessionCommandResult::message("daemon accepted");
    assert_eq!(result.message, "daemon accepted");
    assert!(result.sessions.is_none());
    assert!(result.session_ids.is_none());
}

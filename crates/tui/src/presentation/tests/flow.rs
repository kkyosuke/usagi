//! flow の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

/// うさぎの click は session を訪問したうえで、その agent 自身の tab を開く。
/// 区画（nameplate や余白）の click は従来どおり session の Closeup までで、
/// tab 選択を動かさない。
#[test]
fn clicking_a_usagi_opens_that_agents_tab() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
    };
    let second = TerminalRef {
        terminal_id: TerminalId::new(),
        ..first
    };
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        interaction,
        revision,
        vec![PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second.clone(),
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let runtime_id = AgentRuntimeId::new();
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::RuntimePhase {
        runtime: AgentRuntimeRef {
            agent_runtime_id: runtime_id,
            terminal: second.clone(),
            session_id: Some(session),
        },
        phase: usagi_core::domain::session_lifecycle::AgentPhase::Running,
    }));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));

    // 区画の click（agent 無し）は tab を動かさない。
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let plot_click = GardenClick::Visit {
        workspace,
        session,
        agent: None,
    };
    let _ = runtime.apply_event(AppEvent::GardenClick(plot_click));
    visit_garden_agent(&mut ui, &mut runtime, plot_click);
    assert_eq!(runtime.focused_terminal(), Some(first));

    // うさぎの click は、その runtime を持つ tab を選ぶ。
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let rabbit_click = GardenClick::Visit {
        workspace,
        session,
        agent: Some(runtime_id),
    };
    let _ = runtime.apply_event(AppEvent::GardenClick(rabbit_click));
    visit_garden_agent(&mut ui, &mut runtime, rabbit_click);
    assert_eq!(runtime.focused_terminal(), Some(second.clone()));

    // 押した瞬間に終了していたうさぎは、無関係な tab を選ばない（選択はそのまま）。
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let gone = GardenClick::Visit {
        workspace,
        session,
        agent: Some(AgentRuntimeId::new()),
    };
    let _ = runtime.apply_event(AppEvent::GardenClick(gone));
    visit_garden_agent(&mut ui, &mut runtime, gone);
    assert_eq!(runtime.focused_terminal(), Some(second));

    // Dismiss は訪問ですらないので、agent の焦点も動かさない。
    visit_garden_agent(&mut ui, &mut runtime, GardenClick::Dismiss);
}

#[test]
fn drawer_new_root_completion_commits_one_selected_exact_tab_across_reopen() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let continuation = AgentContinuationRef::new();
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::new(),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(
        runtime
            .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
            .is_empty()
    );
    let effects = runtime.handle_key(Key::Enter);
    let [
        effect @ Effect::LaunchAgent {
            session: None,
            operation_id,
            profile: Some(profile),
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("drawer confirmation must emit one explicit root launch: {effects:?}");
    };
    assert_eq!(profile.as_str(), "claude");
    runtime.on_effect(effect);
    let mut pending = std::collections::HashMap::from([(*operation_id, Target::Root(workspace))]);

    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Agent {
                operation: *operation_id,
                result: Ok(AgentPaneAdmission {
                    terminal: terminal.clone(),
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

    assert!(pending.is_empty());
    assert_eq!(runtime.state().director_launching(), None);
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    let intent = durable.lock().unwrap();
    assert_eq!(intent.targets.len(), 1);
    assert_eq!(intent.targets[0].session_id, None);
    assert_eq!(intent.targets[0].selected, Some(continuation));
    assert_eq!(intent.targets[0].tabs.len(), 1);
    assert!(intent.targets[0].tabs[0].terminal.fences(&terminal));
    drop(intent);

    let tabs = runtime.active_pane().tabs().to_vec();
    let _ = runtime.handle_key(Key::Escape);
    assert!(runtime.state().director_drawer_open());
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    let _ = runtime.handle_key(Key::Escape);
    assert!(!runtime.state().director_drawer_open());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.active_pane().tabs(), tabs.as_slice());
    assert_eq!(
        mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|mutation| matches!(
                mutation,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: actual,
                    select: true,
                    ..
                } if *actual == continuation
            ))
            .count(),
        1
    );
}

#[test]
fn direct_controller_entry_uses_the_resolved_workspace_settings() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char(':'),
        Key::Char('i'),
        Key::Escape,
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
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };
    let settings = usagi_core::domain::settings::Settings {
        modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode::Prompt,
        ..usagi_core::domain::settings::Settings::default()
    };

    assert_eq!(
        run_workspace_controller_with_backend_and_settings(
            &mut term,
            snapshot("direct"),
            &mut factory,
            &settings,
        )
        .unwrap(),
        Exit::Quit
    );
    assert!(term.frames.iter().any(|frame| {
        let frame = frame.join("\n");
        frame.contains("Overview") && frame.contains("Enter: run   Esc: close")
    }));
}

/// #554 acceptance. An open form scans immediately — so the very first frame
/// that shows the caret can already reject a known collision — and then no
/// more than once per cadence period however long it stays open.
#[test]
fn an_open_create_form_scans_on_open_and_then_at_the_cadence_ceiling() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut hint = SessionWorktreeHint::new(counting_scan(&scans));
    let workspace = std::path::Path::new("/tmp/demo");

    // The frame that opens the form.
    assert_eq!(hint.names(true, workspace, at_tick(0)), ["stale-worktree"]);
    assert_eq!(scans.load(Ordering::SeqCst), 1);

    // Five seconds of 16ms ticks with the form left open.
    let ticks = 313;
    for tick in 1..ticks {
        assert_eq!(
            hint.names(true, workspace, at_tick(tick)),
            ["stale-worktree"]
        );
    }

    let elapsed = at_tick(ticks - 1);
    let ceiling =
        usize::try_from(1 + elapsed.as_millis() / SessionWorktreeHint::CADENCE.as_millis())
            .expect("the ceiling of a five second run fits a usize");
    let scanned = scans.load(Ordering::SeqCst);
    assert!(
        scanned <= ceiling,
        "{scanned} scans over {elapsed:?} exceeds the cadence ceiling of {ceiling}"
    );
    assert_eq!(scanned, 10);
}

/// Reopening the form is the moment the hint matters most, so it always
/// rescans — even when the previous scan is still inside the cadence window.
#[test]
fn reopening_the_create_form_rescans_inside_the_cadence_window() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut hint = SessionWorktreeHint::new(counting_scan(&scans));
    let workspace = std::path::Path::new("/tmp/demo");

    assert_eq!(hint.names(true, workspace, at_tick(0)), ["stale-worktree"]);
    assert_eq!(hint.names(true, workspace, at_tick(1)), ["stale-worktree"]);
    assert_eq!(scans.load(Ordering::SeqCst), 1);

    assert!(hint.names(false, workspace, at_tick(2)).is_empty());
    assert_eq!(hint.names(true, workspace, at_tick(3)), ["stale-worktree"]);
    assert_eq!(scans.load(Ordering::SeqCst), 2);
}

/// #554 acceptance for the entry screens. They have no clock and no
/// background lane, so an idle Welcome must draw exactly once however long
/// the terminal keeps ticking.
#[test]
fn idle_entry_screen_ticks_draw_nothing_after_the_first_frame() {
    let mut keys = vec![Key::Other; 60];
    keys.push(Key::Char('q'));
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(
        term.frames.len(),
        1,
        "an idle Welcome rebuilt its frame on a tick"
    );
}

/// The entry gate still redraws the moment the form or the screen changes,
/// so input latency is unaffected: every tick that carries a change draws,
/// and only the empty ones in between are skipped.
#[test]
fn entry_screen_input_redraws_immediately() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Other,
        Key::Down,
        Key::Other,
        Key::Enter,
        Key::Other,
        Key::Escape,
        Key::Other,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    // Welcome, the selection move, the screen it opens, and Welcome again:
    // one frame per input that changed something, none for the four
    // interleaved ticks.
    assert_eq!(term.frames.len(), 4);
}

#[test]
fn direct_controller_entry_binds_workspace_config_settings() {
    let mut keys = vec![Key::Char(':')];
    keys.extend("config".chars().map(Key::Char));
    keys.extend([
        Key::Enter,
        Key::Quit,
        Key::CtrlQ,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut term = FakeTerminal::with_keys(&keys);
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
    let mut settings = WorkspaceBindingSettingsPort::default();

    assert_eq!(
        run_workspace_controller_with_backend_and_config(
            &mut term,
            snapshot("direct-config"),
            &mut factory,
            &mut settings,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(settings.selected, vec![PathBuf::from("/tmp/direct-config")]);
    assert!(term.frames.iter().any(|frame| {
        let frame = frame.join("\n");
        frame.contains("Config")
            && frame.contains("Agent")
            && !frame.contains("Scope:")
            && frame.contains("direct-config")
    }));
}

#[test]
fn direct_deck_entry_uses_the_shared_workspace_composition() {
    let snapshot = snapshot("direct-deck");
    let registry = vec![snapshot.workspace.clone()];
    let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, Key::Char('y')]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
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
        run_workspace_deck(
            &mut term,
            crate::presentation::WorkspaceDeckRun::new(
                snapshot,
                &registry,
                &mut loader,
                &mut factory,
                &mut settings,
            )
            .with_available_models(AvailableAgentModels::all()),
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(settings.selected, vec![PathBuf::from("/tmp/direct-deck")]);
}

#[test]
fn memory_intent_port_fences_an_idempotent_close_before_reopen() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let mut durable = AgentTabIntent::empty(workspace);
    durable.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation,
        terminal: scoped_terminal_ref(workspace, None),
        select: true,
    });
    durable.revision = 1;
    durable.apply(AgentTabIntentMutation::Dismiss { continuation });
    durable.revision = 2;
    let shared = Arc::new(Mutex::new(durable));
    let mut port = MemoryIntentPort {
        state: Arc::clone(&shared),
        mutations: Arc::new(Mutex::new(Vec::new())),
    };

    let close = port
        .mutate(
            workspace,
            1,
            AgentTabIntentMutation::Dismiss { continuation },
        )
        .unwrap();
    assert!(close.cas_conflict);
    assert_eq!(close.intent.revision, 3);

    let reopen = port
        .mutate(
            workspace,
            2,
            AgentTabIntentMutation::Reopen { continuation },
        )
        .unwrap();
    assert!(reopen.cas_conflict);
    assert!(!reopen.mutation_applied);
    assert!(reopen.intent.dismissed.contains(&continuation));
    assert_eq!(reopen.intent.revision, 3);
    assert_eq!(shared.lock().unwrap().revision, 3);

    // A deferred close merges the same way: the exact fence is recorded
    // under the conflict and a repeat still advances the revision.
    let unobserved = scoped_terminal_ref(workspace, None);
    let deferred = port
        .mutate(
            workspace,
            1,
            AgentTabIntentMutation::DismissTerminalAndSelect {
                terminal: unobserved.clone(),
                session_id: None,
                selected: Some(continuation),
            },
        )
        .unwrap();
    assert!(deferred.cas_conflict);
    assert!(deferred.intent.dismissed_terminals.contains(&unobserved));
    assert_eq!(deferred.intent.revision, 4);
    let repeated = port
        .mutate(
            workspace,
            deferred.intent.revision,
            AgentTabIntentMutation::DismissTerminal {
                terminal: unobserved,
            },
        )
        .unwrap();
    assert!(!repeated.cas_conflict);
    assert_eq!(repeated.intent.revision, 5);
}

#[test]
#[allow(clippy::too_many_lines)] // Close and reopen rollback use the same failure fixture.
fn persistence_failures_leave_close_and_reopen_ui_unchanged_with_typed_notice() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let continuation = history.continuation;
    let terminal = history.last_terminal.clone();
    let open_intent = AgentTabIntent::empty(workspace);
    let durable = Arc::new(Mutex::new(open_intent));
    let attempts = Arc::new(AtomicUsize::new(0));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
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
            Box::new(FailingIntentPort {
                state: Arc::clone(&durable),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: None,
            interrupted: vec![history],
        }],
    ));
    let _ = runtime.select_tab(TabDirection::Next);

    crate::presentation::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert!(runtime.focused_terminal().is_none());
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );

    let mut closed_intent = AgentTabIntent::empty(workspace);
    closed_intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal,
        select: true,
    });
    closed_intent.apply(AgentTabIntentMutation::Dismiss { continuation });
    let closed = Arc::new(Mutex::new(closed_intent));
    let closed_bytes = serde_json::to_vec(&*closed.lock().unwrap()).unwrap();
    let reopen_attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&closed),
                error: AgentTabIntentError::ReadOnlySchema,
                attempts: Arc::clone(&reopen_attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
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

    assert!(runtime.active_pane().tabs().is_empty());
    assert_eq!(reopen_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*closed.lock().unwrap()).unwrap(),
        closed_bytes
    );
    assert!(closed.lock().unwrap().dismissed.contains(&continuation));
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::ReadOnlySchema.safe_message())
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One user flow covers close, inventory replay, explicit open, and exit cleanup.
fn generic_close_survives_inventory_replay_until_explicit_open() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = scoped_terminal_ref(workspace, Some(session));
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(WheelRecordingPort {
                terminal: terminal.clone(),
                replay: Vec::new(),
                inputs: Arc::new(Mutex::new(Vec::new())),
                input_error: false,
            }),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: durable,
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let completion = |live: bool, fence: (u64, u64), port: Box<dyn AgentCommandPort>| {
        crate::presentation::RestoreCompletion {
            port,
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Terminal,
                live,
            }]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        }
    };

    let first_fence = runtime.restore_fence();
    let first = crate::presentation::apply_restore_completion(
        completion(true, first_fence, Box::new(UnavailableAgentCommandPort)),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        first.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    ui.start_terminal_session(terminal.clone(), terminal_geometry(20, 80));

    crate::presentation::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.contains(&terminal));

    // The same live row may arrive from a restore already queued around the
    // close. Its exact process-local fence keeps the tab closed.
    let replay_fence = runtime.restore_fence();
    let replay = crate::presentation::apply_restore_completion(
        completion(true, replay_fence, first.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        replay.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.contains(&terminal));

    // A completion outside the pending tab's target is rejected by the
    // runtime and must not release the close fence as a side effect.
    let refused_operation = OperationId::new();
    let target = Target::Session(session);
    let _ = runtime.request_pane(target, refused_operation, PaneKind::Terminal);
    let mut refused_pending = std::collections::HashMap::from([(refused_operation, target)]);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Terminal {
                operation: refused_operation,
                result: Ok(scoped_terminal_ref(workspace, Some(SessionId::new()))),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut refused_pending,
        terminal_geometry(20, 80),
    );
    assert!(ui.closed_generic_terminals.contains(&terminal));
    let _ = runtime.fail_pane(target, refused_operation, "wrong scope".to_owned());

    // An immediate `terminal open` may race the shell exit and resolve to
    // that exact daemon terminal. Reject it instead of resurrecting the
    // scrollback the user just closed.
    let operation = OperationId::new();
    let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Terminal {
                operation,
                result: Ok(terminal.clone()),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert_eq!(
        runtime.active_pane().error(),
        Some("terminal is still closing; try again")
    );
    assert!(ui.closed_generic_terminals.contains(&terminal));

    // Coherent exit observation retires the fence. A later open can then
    // accept only the fresh daemon terminal and its blank scrollback.
    let exit_fence = runtime.restore_fence();
    let exited = crate::presentation::apply_restore_completion(
        completion(false, exit_fence, replay.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        exited.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.is_empty());

    let fresh = scoped_terminal_ref(workspace, Some(session));
    let operation = OperationId::new();
    let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Terminal {
                operation,
                result: Ok(fresh.clone()),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );
    assert_eq!(runtime.focused_terminal(), Some(fresh));
}

#[test]
#[allow(clippy::too_many_lines)] // Reorder success and both persistence failures share one stable fixture.
fn reorder_control_commits_agent_lineages_in_the_new_stable_order() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(ScriptedAgentPort {
                terminal: first_terminal.clone(),
                subscription: 9,
                replay: Vec::new(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                mutations: Arc::clone(&mutations),
            }),
        );
    for (continuation, terminal) in [
        (first, first_terminal.clone()),
        (second, second_terminal.clone()),
    ] {
        let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal,
            select: false,
        });
    }
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (interaction, revision) = runtime.restore_fence();
    let _ = runtime.restore_snapshot(
        interaction,
        revision,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second_terminal,
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first_terminal),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: OperationId::new(),
        profile: None,
    });
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending = std::collections::HashMap::new();

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
    assert!(matches!(
        mutations.lock().unwrap().last(),
        Some(AgentTabIntentMutation::Reorder {
            session_id: Some(actual),
            continuations,
        }) if *actual == session && continuations == &[second, first]
    ));

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabPrevious),
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
    assert!(matches!(
        mutations.lock().unwrap().last(),
        Some(AgentTabIntentMutation::Reorder {
            session_id: Some(actual),
            continuations,
        }) if *actual == session && continuations == &[first, second]
    ));
}

/// The classifier already maps both forms of `n` to New and both forms of
/// `f` to Next. Director keeps those actions unchanged; only the workspace
/// terminal retargets New to a new terminal tab.
#[test]
fn drawer_context_keeps_new_and_next_distinct_after_modifier_normalization() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
        DefaultModel::Claude,
    );

    // Closed drawer: every key passes through unchanged.
    for key in [
        Key::Live(LiveTerminalAction::NextTab),
        Key::Live(LiveTerminalAction::DirectorNew),
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Escape,
    ] {
        assert_eq!(retarget_drawer_chords(&runtime, key.clone()), key);
    }

    assert!(
        runtime
            .handle_key(Key::Live(LiveTerminalAction::Director))
            .is_empty()
    );
    assert!(runtime.state().director_drawer_open());

    // Director keeps the normalized actions unchanged.
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::NextTab)),
        Key::Live(LiveTerminalAction::NextTab)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew)),
        Key::Live(LiveTerminalAction::DirectorNew)
    );
    for key in [
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Live(LiveTerminalAction::Director),
        Key::Escape,
        Key::Char('n'),
    ] {
        assert_eq!(retarget_drawer_chords(&runtime, key.clone()), key);
    }

    // New reaches the reducer without a context-specific swap.
    let retargeted = retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew));
    assert!(runtime.handle_key(retargeted).is_empty());
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(DefaultModel::Claude)
    ));

    // Director remains visible behind Shell, but its chord vocabulary no
    // longer applies while Shell owns root input.
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert!(runtime.state().director_drawer_open());
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::NextTab)),
        Key::Live(LiveTerminalAction::NextTab)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew)),
        Key::Live(LiveTerminalAction::NewRootTerminal)
    );
}

#[test]
fn blocking_operations_keep_painting_and_workspace_open_can_be_cancelled() {
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::Escape, Key::Enter]),
        ..ResponsiveLoadingTerminal::default()
    };
    let draw_count = Arc::clone(&term.draw_count);

    let error = run_workspace_loading(&mut term, "Opening workspace…", true, || {
        while draw_count.load(std::sync::atomic::Ordering::Acquire) < 3 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    })
    .expect_err("escape cancels the visible wait");

    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    let painted = term.frames.iter().flatten().cloned().collect::<String>();
    assert!(painted.contains("Opening workspace…"));
    assert!(painted.contains("Cancelling…"));
    assert!(
        term.frames.len() >= 3,
        "the loading surface kept repainting"
    );
    assert_eq!(term.wait_keys, VecDeque::from([Key::Enter]));
}

#[test]
fn production_workspace_ports_open_refresh_and_activate_on_the_worker_path() {
    let mut term = ResponsiveLoadingTerminal::default();
    let mut loader = FakeLoader {
        operation_mode: FakeOperationMode::Background,
        open_snapshot: FakeOpenSnapshot::Empty,
        ..FakeLoader::default()
    };

    let snapshot = open_workspace_responsive(
        &mut term,
        &mut loader,
        Path::new("/tmp/background"),
        "Opening workspace…",
    )
    .expect("background open succeeds");
    activate_workspace_responsive(
        &mut term,
        &mut loader,
        &snapshot.workspace.path,
        "Activating workspace…",
    )
    .expect("background activation succeeds");

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/background")]);
    assert_eq!(loader.refreshed, vec![PathBuf::from("/tmp/background")]);

    let paths = vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")];
    let (_, active, _) = prepare_workspace_deck(&mut term, &mut loader, &paths)
        .expect("background deck preparation succeeds");
    assert_eq!(active.workspace.path, PathBuf::from("/tmp/one"));

    let mut failing_loader = FakeLoader {
        activate_error: Some("activation failed"),
        ..FakeLoader::default()
    };
    let error = prepare_workspace_deck(
        &mut term,
        &mut failing_loader,
        &[PathBuf::from("/tmp/fail")],
    )
    .expect_err("activation failure is propagated");
    assert_eq!(error.to_string(), "activation failed");
}

#[test]
fn run_quits_from_welcome_and_handles_menu_navigation() {
    for keys in [
        vec![Key::Char('q'), Key::Enter],
        vec![Key::Quit],
        vec![Key::Escape],
        vec![Key::Down, Key::Down, Key::Up, Key::Quit],
        vec![Key::Down, Key::Down, Key::Down, Key::Enter],
    ] {
        let mut term = FakeTerminal::with_keys(&keys);
        assert_eq!(
            run(
                &mut term,
                Vec::new(),
                Vec::new(),
                now(),
                &mut FakeLoader::default(),
            )
            .unwrap(),
            Exit::Quit
        );
        assert!(term.frames[0].join("\n").contains("Menu"));
    }
}

#[test]
fn startup_splash_draws_and_paces_every_frame_without_reading_input() {
    let mut term = FakeTerminal::default();

    play_startup_splash(&mut term).unwrap();

    assert_eq!(
        term.frames.len(),
        crate::presentation::views::splash::FRAMES
    );
    assert_eq!(term.waits.len(), crate::presentation::views::splash::FRAMES);
    assert!(
        term.waits
            .iter()
            .all(|wait| *wait == crate::presentation::views::splash::ANIM_TICK)
    );
    assert!(term.keys.is_empty());
}

#[test]
fn run_ignores_unknown_welcome_keys() {
    let keys = [
        Key::Char('z'),
        Key::Left,
        Key::Right,
        Key::Backspace,
        Key::Other,
        Key::Char('q'),
        Key::Enter,
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    // None of these keys changes Welcome, so the gate draws the menu once
    // and every ignored key costs nothing (#554).
    assert_eq!(term.frames.len(), 1);
    assert!(
        term.frames
            .iter()
            .all(|frame| frame.join("\n").contains("Menu"))
    );
}

#[test]
fn entry_help_is_contextual_and_exclusively_owns_input_until_closed() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Help,
        Key::Down,
        // This would open the workspace list if Help did not own input.
        Key::Char('o'),
        Key::Escape,
        Key::Char('q'),
    ]);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();

    assert_eq!(term.frames.len(), 4);
    assert!(
        term.frames[1]
            .join("\n")
            .contains("Keyboard help · Welcome")
    );
    assert!(term.frames[1].join("\n").contains("open Recent card"));
    assert!(
        term.frames[2]
            .join("\n")
            .contains("Keyboard help · Welcome")
    );
    assert!(term.frames[3].join("\n").contains("Menu"));
    assert!(
        term.frames
            .iter()
            .all(|frame| !frame.join("\n").contains("Open Workspace"))
    );
}

#[test]
fn entry_help_resolves_every_entry_surface_and_config_submode() {
    use crate::presentation::Screen;
    use crate::presentation::views::key_help::Context as HelpContext;

    let mut settings = DefaultSettingsPort;
    let config = Config::load(&mut settings);
    let mut open = Open::new(vec![ws("atlas")]);

    for (screen, expected) in [
        (Screen::Welcome, HelpContext::Welcome),
        (Screen::Open, HelpContext::Open),
        (Screen::New, HelpContext::New),
        (Screen::Config, HelpContext::Config),
    ] {
        assert_eq!(
            crate::presentation::entry_help_context(screen, &open, &config, false),
            expected
        );
    }
    assert_eq!(
        crate::presentation::entry_help_context(Screen::Welcome, &open, &config, true),
        HelpContext::MissingWorkspace
    );

    open.request_unregister();
    assert_eq!(
        crate::presentation::entry_help_context(Screen::Open, &open, &config, false),
        HelpContext::OpenUnregister
    );
    open.cancel_unregister();
    open.request_cleanup();
    assert_eq!(
        crate::presentation::entry_help_context(Screen::Open, &open, &config, false),
        HelpContext::OpenCleanup
    );

    let mut team = Config::load(&mut settings);
    for _ in 0..7 {
        let _ = step_config(&mut team, Key::Down, &mut settings);
    }
    let _ = step_config(&mut team, Key::Enter, &mut settings);
    assert_eq!(
        crate::presentation::config_help_context(&team),
        HelpContext::TeamPicker
    );

    let mut environment = Config::load(&mut settings);
    for _ in 0..4 {
        let _ = step_config(&mut environment, Key::Down, &mut settings);
    }
    let _ = step_config(&mut environment, Key::Enter, &mut settings);
    assert_eq!(
        crate::presentation::config_help_context(&environment),
        HelpContext::EnvironmentEditor
    );

    let mut setup =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut setup, Key::Down, &mut settings);
    }
    let _ = step_config(&mut setup, Key::Enter, &mut settings);
    assert_eq!(
        crate::presentation::config_help_context(&setup),
        HelpContext::SessionSetupEditor
    );
}

#[test]
fn welcome_action_maps_every_destination() {
    assert!(matches!(
        welcome_action(MenuAction::Quit),
        WelcomeStep::Quit
    ));
    assert!(matches!(
        welcome_action(MenuAction::Open),
        WelcomeStep::OpenList
    ));
    assert!(matches!(
        welcome_action(MenuAction::OpenRecent(2)),
        WelcomeStep::OpenRecent(2)
    ));
    assert!(matches!(
        welcome_action(MenuAction::New),
        WelcomeStep::NewForm
    ));
    assert!(matches!(
        welcome_action(MenuAction::Config),
        WelcomeStep::ConfigScreen
    ));
}

#[test]
fn config_can_be_opened_from_welcome_or_used_as_the_start() {
    let mut from_welcome =
        FakeTerminal::with_keys(&[Key::Char('c'), Key::Escape, Key::Char('q'), Key::Enter]);
    run(
        &mut from_welcome,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(from_welcome.frames[0].join("\n").contains("Menu"));
    assert!(from_welcome.frames[1].join("\n").contains("Config"));
    assert!(from_welcome.frames[2].join("\n").contains("Menu"));

    let mut direct = FakeTerminal::with_keys(&[Key::Char('x'), Key::Quit]);
    run_from_start(
        &mut direct,
        Vec::new(),
        Vec::new(),
        now(),
        Start::Config,
        &mut FakeLoader::default(),
    )
    .unwrap();
    // `x` changes nothing on Config, so only the entry frame is drawn.
    assert_eq!(direct.frames.len(), 1);
    assert!(
        direct
            .frames
            .iter()
            .all(|frame| frame.join("\n").contains("Config"))
    );
}

#[test]
fn step_config_maps_back_quit_and_stay() {
    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    assert!(matches!(
        step_config(&mut config, Key::Escape, &mut settings),
        ConfigStep::Back
    ));
    assert!(matches!(
        step_config(&mut config, Key::Quit, &mut settings),
        ConfigStep::Quit
    ));
    assert!(matches!(
        step_config(&mut config, Key::Char('x'), &mut settings),
        ConfigStep::Stay
    ));
    assert!(matches!(
        step_config(&mut config, Key::Tab, &mut settings),
        ConfigStep::Stay
    ));
    for key in [
        Key::Up,
        Key::Down,
        Key::Char('j'),
        Key::Left,
        Key::Right,
        Key::CtrlQ,
    ] {
        let _ = step_config(&mut config, key, &mut settings);
    }
}

#[test]
fn step_config_opens_applies_and_cancels_the_team_picker() {
    use crate::presentation::views::config::Field as ConfigField;
    use usagi_core::domain::settings::TeamTemplate;

    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    for _ in 0..7 {
        step_config(&mut config, Key::Down, &mut settings);
    }
    assert_eq!(config.field(), ConfigField::TeamTemplate);
    step_config(&mut config, Key::Right, &mut settings);
    assert_eq!(config.settings().team_template, TeamTemplate::None);

    step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_selecting_team());
    step_config(&mut config, Key::Other, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Up, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Enter, &mut settings);
    assert!(!config.is_selecting_team());
    assert_eq!(config.settings().team_template, TeamTemplate::Flat);

    step_config(&mut config, Key::Enter, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Up, &mut settings);
    step_config(&mut config, Key::Left, &mut settings);
    step_config(&mut config, Key::Tab, &mut settings);
    step_config(&mut config, Key::Escape, &mut settings);
    assert!(!config.is_selecting_team());
    assert_eq!(config.settings().team_template, TeamTemplate::Flat);
}

#[test]
fn step_config_routes_input_to_the_global_environment_editor() {
    use crate::presentation::views::config::Field as ConfigField;

    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    assert_eq!(config.field(), ConfigField::Environment);
    step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_editing_environment());
    step_config(&mut config, Key::Char('C'), &mut settings);
    step_config(&mut config, Key::Paste("=3xy".to_owned()), &mut settings);
    step_config(&mut config, Key::Backspace, &mut settings);
    step_config(&mut config, Key::Left, &mut settings);
    step_config(&mut config, Key::Delete, &mut settings);
    step_config(&mut config, Key::End, &mut settings);
    step_config(&mut config, Key::Enter, &mut settings);
    step_config(
        &mut config,
        Key::Paste("A=1\r\nB=2".to_owned()),
        &mut settings,
    );
    step_config(&mut config, Key::Up, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Home, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::LineEnd, &mut settings);
    step_config(&mut config, Key::LineStart, &mut settings);
    step_config(&mut config, Key::Tab, &mut settings);
    assert!(!config.is_environment_save_focused());
    step_config(&mut config, Key::Other, &mut settings);
    step_config(
        &mut config,
        Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![19],
        },
        &mut settings,
    );
    assert!(!config.is_editing_environment());
    assert_eq!(config.settings().env["A"], "1");
    assert_eq!(config.settings().env["B"], "2");
    assert_eq!(config.settings().env["C"], "3");

    step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_editing_environment());
    step_config(&mut config, Key::Escape, &mut settings);
    assert!(!config.is_editing_environment());
}

#[test]
fn step_config_saves_the_workspace_environment_from_its_save_action() {
    let mut settings = DefaultSettingsPort;
    let mut config =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Enter, &mut settings);
    step_config(&mut config, Key::Char('A'), &mut settings);
    step_config(&mut config, Key::Paste("=1".to_owned()), &mut settings);
    step_config(&mut config, Key::Tab, &mut settings);
    assert!(config.is_environment_save_focused());
    step_config(&mut config, Key::Enter, &mut settings);

    assert!(!config.is_editing_environment());
    assert_eq!(config.settings().env["A"], "1");
}

#[test]
fn step_config_saves_only_from_the_dirty_save_row() {
    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Stay
    ));
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    // Enter on the dirty Save row begins the save flow (loading).
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Save
    ));
    // A second Enter while Saving is a no-op, so it stays on the screen.
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Stay
    ));
}

#[test]
fn overview_config_saves_the_current_workspace_and_returns_to_home() {
    let mut keys = vec![Key::Char('o'), Key::Enter, Key::Char(':')];
    keys.extend("config".chars().map(Key::Char));
    keys.extend([
        Key::Enter,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Right,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            vec![ws("project")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(settings.selected, vec![PathBuf::from("/tmp/project")]);
    assert_eq!(settings.saves.len(), 1);
    assert_eq!(settings.saves[0].0, SettingsScope::Workspace);
    assert!(!settings.saves[0].1.issue_enabled);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    let config = frames
        .iter()
        .position(|frame| {
            frame.contains("Config") && frame.contains("Agent") && !frame.contains("Scope:")
        })
        .expect("workspace Config is rendered");
    assert!(frames[config].contains("project"));
    assert!(!frames[config].contains("Overview"));
    let done = frames
        .iter()
        .position(|frame| frame.contains("Config") && frame.contains("[ done ]"))
        .expect("workspace Config shows done before closing");
    let returned_home = frames
        .iter()
        .skip(done + 1)
        .any(|frame| frame.contains("project") && !frame.contains("Config"));
    assert!(config < done && returned_home);
    assert_eq!(term.waits, config_save_waits(true));
}

#[test]
fn screen_graph_binds_settings_for_open_recent_and_new_entries() {
    let cases = [
        (
            vec![Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')],
            vec![ws("open")],
            Vec::new(),
            PathBuf::from("/tmp/open"),
        ),
        (
            vec![Key::Char('1'), Key::CtrlQ, Key::Char('y')],
            Vec::new(),
            vec![recent("recent")],
            PathBuf::from("/tmp/recent"),
        ),
        (
            vec![
                Key::Char('e'),
                Key::Right,
                Key::Down,
                Key::Char('x'),
                Key::Enter,
                Key::CtrlQ,
                Key::Char('y'),
            ],
            Vec::new(),
            Vec::new(),
            PathBuf::from("/tmp/x"),
        ),
    ];

    for (keys, workspaces, recent, expected) in cases {
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = WorkspaceBindingSettingsPort::default();
        let mut sessions = UnavailableSessionCommandPortFactory;
        assert_eq!(
            run_with_settings(
                &mut term,
                workspaces,
                recent,
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );
        assert_eq!(settings.selected, vec![expected]);
    }
}

#[test]
fn production_config_and_source_writes_use_the_responsive_worker_path() {
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut config = Config::load(&mut settings);
    let _ = step_config(&mut config, Key::Right, &mut settings);
    for _ in 0..11 {
        let _ = step_config(&mut config, Key::Down, &mut settings);
    }
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Save
    ));
    let mut term = ResponsiveLoadingTerminal::default();
    assert!(
        save_config_responsive(&mut term, &mut config, &mut settings, None)
            .expect("responsive settings save")
    );

    let mut environment =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    let _ = step_config(&mut environment, Key::Down, &mut settings);
    let _ = step_config(&mut environment, Key::Enter, &mut settings);
    let _ = step_config(
        &mut environment,
        Key::Paste("A=1".to_owned()),
        &mut settings,
    );
    let _ = step_config(&mut environment, Key::Tab, &mut settings);
    assert!(matches!(
        step_config(&mut environment, Key::Enter, &mut settings),
        ConfigStep::SaveSource
    ));
    assert!(save_config_source_responsive(
        &mut term,
        &mut environment,
        &mut settings,
    ));

    let mut setup =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut setup, Key::Down, &mut settings);
    }
    let _ = step_config(&mut setup, Key::Enter, &mut settings);
    let _ = step_config(
        &mut setup,
        Key::Paste("cargo fetch\ncargo test".to_owned()),
        &mut settings,
    );
    let _ = step_config(&mut setup, Key::Tab, &mut settings);
    assert!(matches!(
        step_config(&mut setup, Key::Enter, &mut settings),
        ConfigStep::SaveSource
    ));
    assert!(save_config_source_responsive(
        &mut term,
        &mut setup,
        &mut settings,
    ));

    assert_eq!(settings.saves, 1);
    assert_eq!(settings.environment_saves, 1);
    assert_eq!(settings.setup_saves, 1);
    assert_eq!(settings.setup_commands, ["cargo fetch", "cargo test"]);
    assert_eq!(environment.settings().env["A"], "1");
    let painted = term.frames.iter().flatten().cloned().collect::<String>();
    assert!(painted.contains("Saving settings…"));
    assert!(painted.contains("Saving environment…"));
    assert!(painted.contains("Saving session setup…"));
}

#[test]
fn background_environment_shortcut_reaches_both_config_surfaces() {
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut config = Config::load(&mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Enter, &mut settings);
    let save = Key::Management {
        action: AppKey::SaveRoles,
        passthrough: vec![0x13],
    };
    assert!(matches!(
        step_config(&mut config, save.clone(), &mut settings),
        ConfigStep::SaveSource
    ));
    assert!(matches!(
        step_workspace_config(&mut config, save, &mut settings),
        WorkspaceConfigStep::SaveSource
    ));
}

#[test]
fn workspace_config_dispatches_background_environment_save() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut term = FakeTerminal::with_keys(&[
        Key::Down,
        Key::Enter,
        Key::Paste("WORKSPACE=1".to_owned()),
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

    assert_eq!(settings.environment_saves, 1);
}

#[test]
fn full_config_dispatches_background_environment_save() {
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut loader = FakeLoader::default();
    let mut sessions = UnavailableSessionCommandPortFactory;
    let mut term = FakeTerminal::with_keys(&[
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::Paste("GLOBAL=1".to_owned()),
        Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![0x13],
        },
        Key::Escape,
        Key::Quit,
    ]);

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Config,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(settings.environment_saves, 1);
}

#[test]
fn workspace_config_handles_back_and_failed_save_without_leaving_drafts() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort::default();
    let mut back = FakeTerminal::with_keys(&[Key::Escape]);
    run_workspace_config(
        &mut back,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    let keys = WORKSPACE_CONFIG_SAVE_KEYS
        .iter()
        .cloned()
        .chain(std::iter::once(Key::Escape))
        .collect::<Vec<_>>();
    let mut failed = FakeTerminal::with_keys(&keys);
    let mut failing_settings = RecordingSettingsPort {
        fail_save: true,
        ..RecordingSettingsPort::default()
    };
    run_workspace_config(
        &mut failed,
        &mut failing_settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();
    assert_eq!(failed.waits, config_save_waits(false));
    assert!(
        failed
            .frames
            .iter()
            .any(|frame| frame.join("\n").contains("Save failed"))
    );
}

#[test]
fn background_workspace_config_save_keeps_one_modal_visible_until_done() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut term = ResponsiveLoadingTerminal {
        keys: VecDeque::from(WORKSPACE_CONFIG_SAVE_KEYS),
        ..ResponsiveLoadingTerminal::default()
    };

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    assert_eq!(settings.saves, 1);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(frames.iter().all(|frame| frame.contains("Config")));
    assert!(
        frames
            .iter()
            .all(|frame| !frame.contains("Saving settings…"))
    );
    assert!(frames.iter().any(|frame| frame.contains("[ done ]")));
}

#[test]
fn workspace_config_swallows_quit_keys_until_escape() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort::default();
    let mut term = FakeTerminal::with_keys(&[Key::Quit, Key::CtrlQ, Key::Char('q'), Key::Escape]);

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    assert_eq!(term.frames.len(), 4);
    assert!(
        term.frames
            .iter()
            .all(|frame| frame.join("\n").contains("Config"))
    );
}

#[test]
fn workspace_config_help_toggles_and_exclusively_owns_input() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort::default();
    let mut term = FakeTerminal::with_keys(&[
        Key::Help,
        Key::Quit,
        Key::Help,
        Key::Help,
        Key::Escape,
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

    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 6);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.contains("Keyboard help · Config"))
            .count(),
        3
    );
    assert!(!frames.last().unwrap().contains("Keyboard help"));
}

#[test]
fn config_save_waves_then_shows_done_and_returns_home_on_its_own() {
    let keys: Vec<Key> = CONFIG_SAVE_KEYS
        .iter()
        .cloned()
        .chain(std::iter::once(Key::Quit)) // now on Welcome; quit to end the loop
        .collect();
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = RecordingSettingsPort::default();
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Config,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    // Exactly one write, one complete wave, and one confirmation dwell —
    // the screen returned home on the timer, with no extra key press.
    assert_eq!(settings.saves, 1);
    assert_eq!(term.waits, config_save_waits(true));

    // Frames appear in order: an animated Save caption, then `done`, then
    // the Welcome `Menu` reached without a key press.
    let joined: Vec<String> = term.frames.iter().map(|frame| frame.join("\n")).collect();
    let done = joined
        .iter()
        .position(|frame| frame.contains("[ done ]"))
        .expect("a done confirmation frame is drawn");
    let wave = &term.frames[done - crate::presentation::views::config::SAVE_WAVE_FRAMES..done];
    assert!(wave.iter().all(|frame| {
        frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
            .contains("[ Save ]")
    }));
    assert!(wave.windows(2).all(|frames| frames[0] != frames[1]));
    let menu = joined
        .iter()
        .rposition(|frame| frame.contains("Menu"))
        .expect("the Welcome menu is drawn after returning home");
    assert!(done < menu);
}

#[test]
fn config_save_failure_stays_on_the_screen_without_dwelling_or_returning() {
    let keys: Vec<Key> = CONFIG_SAVE_KEYS
        .iter()
        .cloned()
        .chain([Key::Escape, Key::Quit]) // still on Config; Esc back, then quit
        .collect();
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = RecordingSettingsPort {
        fail_save: true,
        ..RecordingSettingsPort::default()
    };
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Config,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    // A failed write still animates while pending, but neither dwells on
    // `done` nor auto-returns.
    assert_eq!(settings.saves, 0);
    assert_eq!(term.waits, config_save_waits(false));

    let joined: Vec<String> = term.frames.iter().map(|frame| frame.join("\n")).collect();
    // The error is surfaced on the Config screen and no `done` confirmation
    // is ever shown.
    assert!(joined.iter().any(|frame| frame.contains("Save failed")));
    assert!(joined.iter().all(|frame| !frame.contains("[ done ]")));
}

#[test]
fn new_form_opens_edits_and_returns_to_welcome() {
    let keys = [
        Key::Char('e'),
        Key::Down,
        Key::Char('a'),
        Key::Backspace,
        Key::Escape,
        Key::Char('q'),
        Key::Enter,
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[0].join("\n").contains("Menu"));
    assert!(
        term.frames[1..5]
            .iter()
            .all(|frame| frame.join("\n").contains("New Project"))
    );
    assert!(term.frames[5].join("\n").contains("Menu"));
}

#[test]
fn step_new_handles_every_edit_and_exit_key() {
    let mut form = New::default();
    assert!(matches!(step_new(&mut form, Key::Down), NewStep::Stay));
    assert_eq!(form.focus(), Field::Url);
    assert!(matches!(step_new(&mut form, Key::Up), NewStep::Stay));
    assert_eq!(form.focus(), Field::Mode);
    step_new(&mut form, Key::Right);
    assert_eq!(form.mode(), Mode::Existing);
    step_new(&mut form, Key::Left);
    assert_eq!(form.mode(), Mode::Clone);
    step_new(&mut form, Key::Down);
    step_new(&mut form, Key::Char('a'));
    step_new(&mut form, Key::Char('b'));
    step_new(&mut form, Key::Left);
    step_new(&mut form, Key::Right);
    step_new(&mut form, Key::Backspace);
    for key in [
        Key::Home,
        Key::End,
        Key::LineStart,
        Key::LineEnd,
        Key::SelectLeft,
        Key::SelectRight,
        Key::SelectHome,
        Key::SelectEnd,
        Key::Delete,
        Key::Tab,
        Key::CtrlD,
        Key::Live(LiveTerminalAction::NextTab),
        Key::Click { column: 0, row: 0 },
        Key::Passthrough(Vec::new()),
    ] {
        let _ = step_new(&mut form, key);
    }
    assert_eq!(form.url(), "a");
    // Enter with a still-incomplete Clone form (no Location) validates,
    // surfaces the field error as a notice, and stays on the form.
    assert!(matches!(step_new(&mut form, Key::Enter), NewStep::Stay));
    assert_eq!(form.notice(), Some("clone location is required"));
    assert!(matches!(step_new(&mut form, Key::Other), NewStep::Stay));
    assert!(matches!(step_new(&mut form, Key::Escape), NewStep::Back));
    assert!(matches!(step_new(&mut form, Key::Quit), NewStep::Quit));
    assert!(matches!(step_new(&mut form, Key::CtrlQ), NewStep::Quit));
}

#[test]
fn step_new_paste_inserts_the_pasted_text_into_the_focused_field() {
    let mut form = New::default();
    step_new(&mut form, Key::Down); // focus the Url field
    assert!(matches!(
        step_new(
            &mut form,
            Key::Paste("https://example.com/repo.git".to_owned()),
        ),
        NewStep::Stay
    ));
    assert_eq!(form.url(), "https://example.com/repo.git");
}

#[test]
fn step_open_paste_appends_its_text_to_the_filter() {
    let mut open = Open::new(vec![ws("alpha")]);
    assert!(matches!(
        step_open(&mut open, Key::Paste("alp".to_owned())),
        OpenStep::Stay
    ));
    assert_eq!(open.filter(), "alp");
}

#[test]
fn step_welcome_ignores_a_bracketed_paste() {
    let mut welcome = crate::presentation::Welcome::new(Vec::new());
    assert!(matches!(
        crate::presentation::step_welcome(&mut welcome, Key::Paste("x".to_owned())),
        crate::presentation::WelcomeStep::Stay
    ));
}

#[test]
fn step_new_enter_creates_once_every_required_field_is_present() {
    let mut form = New::default();
    step_new(&mut form, Key::Down); // Url
    for ch in "https://example.com/owner/repo.git".chars() {
        step_new(&mut form, Key::Char(ch));
    }
    step_new(&mut form, Key::Down); // Location
    for ch in "/projects".chars() {
        step_new(&mut form, Key::Char(ch));
    }
    // Directory は URL から導出済み。Enter で検証済みの Create を返す。
    let step = step_new(&mut form, Key::Enter);
    assert!(matches!(step, NewStep::Create(NewRequest::Clone { .. })));
}

#[test]
fn new_project_notice_collapses_git_stderr_to_one_safe_line() {
    // 空メッセージは汎用の一行へフォールバックする。
    assert_eq!(
        new_project_notice(&io::Error::other(String::new())),
        "could not create the project"
    );
    // 複数行の stderr は先頭行だけを trim して残す。
    let multi = io::Error::other("fatal: repository not found\nhint: check the URL");
    assert_eq!(new_project_notice(&multi), "fatal: repository not found");
    // 長い行は省略記号付きで切り詰める。
    let long = io::Error::other("x".repeat(200));
    let notice = new_project_notice(&long);
    assert_eq!(notice.chars().count(), 72);
    assert!(notice.ends_with('…'));
}

#[test]
fn step_new_inserts_navigation_letters_instead_of_treating_them_as_movement() {
    let mut form = New::default();
    step_new(&mut form, Key::Down); // Url
    step_new(&mut form, Key::Char('j'));
    step_new(&mut form, Key::Char('k'));
    assert_eq!(form.focus(), Field::Url);
    assert_eq!(form.url(), "jk");
}

#[test]
fn quitting_from_new_exits_the_runtime() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('e'), Key::Quit]);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[1].join("\n").contains("New Project"));
}

#[test]
fn new_form_enter_creates_a_workspace_and_opens_it() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'), // Welcome → New
        Key::Right,     // Clone → Existing
        Key::Down,      // focus the directory path
        Key::Char('x'), // path "x"; the name derives "x"
        Key::Enter,     // valid → create and open the workspace
        Key::CtrlQ,     // leave the workspace…
        Key::Char('y'), // …confirm
    ]);
    let mut loader = FakeLoader::default();
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // Enter dispatched exactly one create carrying the validated request.
    assert_eq!(
        loader.created,
        vec![NewRequest::Existing {
            path: PathBuf::from("x"),
            name: "x".to_owned(),
        }]
    );
    // The freshly created workspace opened on the same terminal.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("x-session"))
    );
}

#[test]
fn hung_new_create_keeps_ticks_resize_escape_and_quit_responsive() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Other,
        Key::Resize,
        Key::Enter,
        Key::Escape,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        hold_create: true,
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // The second Enter is coalesced while the sole operation is pending.
    assert_eq!(loader.created.len(), 1);
    // Wake-up and resize each advance the spinner and redraw the New frame.
    let loading_frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .filter(|frame| frame.contains("creating workspace"))
        .collect::<Vec<_>>();
    assert!(loading_frames.len() >= 3, "{loading_frames:?}");
    assert_ne!(loading_frames[0], loading_frames[1]);
    // Escape left the hung operation behind and Welcome processed Quit.
    assert!(term.frames.last().unwrap().join("\n").contains("Menu"));
}

#[test]
fn loading_new_can_quit_without_waiting_for_create_completion() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        hold_create: true,
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.created.len(), 1);
}

#[test]
fn cancelled_create_completion_after_reentry_never_opens_the_workspace() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Escape,
        Key::Char('e'),
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        hold_create: true,
        release_after_polls: Some(2),
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.created.len(), 1);
    assert!(
        term.frames
            .iter()
            .all(|frame| !frame.join("\n").contains("Overview"))
    );
    let last_new = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("New Project"))
        .expect("re-entered New frame");
    assert!(last_new.join("\n").contains('x'));
}

#[test]
fn reentered_new_refuses_resubmit_until_cancelled_failure_completes() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Escape,
        Key::Char('e'),
        Key::Enter,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        fail: true,
        hold_create: true,
        release_after_polls: Some(3),
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.created.len(), 1);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("previous creation is still finishing"))
    );
    assert!(frames.iter().any(|frame| frame.contains("open failed")));
}

#[test]
fn stale_and_duplicate_create_completions_open_success_exactly_once() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut loader = FakeLoader {
        completion_noise: true,
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            Vec::new(),
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
    assert_eq!(loader.created.len(), 1);
    assert_eq!(factory.drops_at_create.len(), 1);
}

/// A workspace the daemon does not serve must not be shown: its session list
/// would be the daemon's workspace under the opened workspace's name (#549).
/// Both switcher entries stay up with the refusal instead of tearing the TUI
/// down, so the workspace that *is* served can be chosen next.
#[test]
fn a_refused_workspace_keeps_the_switcher_open_with_the_reason() {
    const REFUSAL: &str = "cannot open /tmp/recent: this daemon does not serve the selected workspace; \
             this daemon serves the workspace /tmp/served. \
             Stop it with `usagi daemon stop`, then start usagi in /tmp/recent.";

    // Welcome's Recent entry: the refusal shows on Welcome, and no workspace
    // screen is drawn for the workspace that was refused.
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        ..FakeLoader::default()
    };
    assert_eq!(
        run(
            &mut term,
            Vec::new(),
            vec![recent("recent")],
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/recent")]);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    let welcome = frames
        .iter()
        .rev()
        .find(|frame| frame.contains("Menu"))
        .expect("the switcher stays on screen");
    // Wrapped over several lines, so the reason and the recovery step are both
    // present rather than clipped at the terminal width.
    assert!(contains_wrapped(welcome, REFUSAL), "{welcome}");
    assert!(!frames.iter().any(|frame| frame.contains("Overview")));

    // The Open list: same contract, presented on the list itself.
    let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Escape, Key::Quit]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        ..FakeLoader::default()
    };
    assert_eq!(
        run(
            &mut term,
            vec![ws("served")],
            Vec::new(),
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/served")]);
    let open_list = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .rev()
        .find(|frame| frame.contains("Open Workspace"))
        .expect("the Open list stays on screen");
    assert!(contains_wrapped(&open_list, REFUSAL), "{open_list}");
}

#[test]
fn only_a_refusal_keeps_the_switcher_open() {
    // Every other failure still propagates: staying on the list would not
    // help, and the caller reports it.
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    let error = run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut loader,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "open failed");

    let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Quit]);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    let error = run(
        &mut term,
        vec![ws("served")],
        Vec::new(),
        now(),
        &mut loader,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "open failed");
}

#[test]
fn new_form_enter_keeps_the_draft_and_shows_a_notice_when_creation_fails() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter, // create fails
        Key::Quit,  // then quit from the still-open New form
    ]);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // The create was attempted once and the runtime stayed on the New form.
    assert_eq!(loader.created.len(), 1);
    let last_new = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("New Project"))
        .expect("still on the New screen after a failed create");
    let text = last_new.join("\n");
    assert!(text.contains("open failed")); // the failure notice
    assert!(text.contains('x')); // the draft path is retained
}

#[test]
fn new_form_keeps_the_draft_when_worker_dispatch_fails() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        dispatch_error: Some("worker dispatch failed"),
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert!(loader.created.is_empty());
    let last_new = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("New Project"))
        .expect("still on the New screen after dispatch failed");
    let text = last_new.join("\n");
    assert!(text.contains("worker dispatch failed"));
    assert!(text.contains('x'));
}

#[test]
fn new_form_recovers_after_an_existing_workspace_rejection_and_retries() {
    // The first create is rejected as if the workspace already existed; the
    // user edits the path and the second create succeeds and opens.
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'), // Welcome → New
        Key::Right,     // Clone → Existing
        Key::Down,      // focus the directory path
        Key::Char('x'), // path "x"
        Key::Enter,     // create #1 → rejected (already registered)
        Key::Char('y'), // fix the path → "xy" (draft was retained)
        Key::Enter,     // create #2 → succeeds and opens
        Key::CtrlQ,     // leave the workspace…
        Key::Char('y'), // …confirm
    ]);
    let mut loader = FakeLoader {
        create_failures: 1,
        ..FakeLoader::default()
    };
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // Two attempts: the rejected "x" and the corrected "xy".
    assert_eq!(
        loader.created,
        vec![
            NewRequest::Existing {
                path: PathBuf::from("x"),
                name: "x".to_owned(),
            },
            NewRequest::Existing {
                path: PathBuf::from("xy"),
                name: "xy".to_owned(),
            },
        ]
    );
    // The rejection surfaced a safe notice on the retained New form…
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("already a registered workspace"))
    );
    // …and the corrected retry opened the freshly created workspace.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("xy-session"))
    );
}

#[test]
fn new_form_enter_clones_and_opens_the_workspace() {
    let mut keys = vec![Key::Char('e'), Key::Down]; // New → focus Url
    keys.extend("https://example.com/o/repo.git".chars().map(Key::Char));
    keys.push(Key::Down); // focus Location
    keys.extend("/tmp".chars().map(Key::Char));
    // Directory は URL から "repo" が導出済み。
    keys.extend([Key::Enter, Key::CtrlQ, Key::Char('y')]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(
        loader.created,
        vec![NewRequest::Clone {
            repository: "https://example.com/o/repo.git".to_owned(),
            destination: PathBuf::from("/tmp").join("repo"),
            branch: None,
        }]
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("repo-session"))
    );
}

#[test]
fn missing_open_selection_confirms_registry_only_removal() {
    let alpha = ws("alpha");
    let mut term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('y'), Key::Quit]);
    let mut loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };

    assert_eq!(
        run(
            &mut term,
            vec![alpha.clone()],
            Vec::new(),
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );

    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames.iter().any(|frame| {
            frame.contains("Workspace not found")
                && frame.contains("/tmp/alpha")
                && frame.contains("remove")
                && frame.contains("cancel")
                && frame.contains("No workspace data is deleted")
        }),
        "missing-workspace prompt was not rendered: {frames:#?}"
    );
    assert_eq!(loader.opened, Vec::<PathBuf>::new());
    assert_eq!(loader.cleanup_calls, 1);
    assert_eq!(loader.cleanup_candidates, vec![vec![alpha.path]]);
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("No workspaces yet"))
    );
}

#[test]
fn missing_unite_member_is_preflighted_before_any_workspace_opens() {
    let alpha = ws("alpha");
    let beta = ws("beta");
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Tab,
        Key::Char(' '),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Char('y'),
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        missing: vec![alpha.path.clone(), beta.path.clone()],
        cleanup_removed: vec![alpha.path.clone(), beta.path.clone()],
        ..FakeLoader::default()
    };

    run(
        &mut term,
        vec![alpha.clone(), beta.clone()],
        Vec::new(),
        now(),
        &mut loader,
    )
    .unwrap();

    assert_eq!(loader.opened, Vec::<PathBuf>::new());
    assert_eq!(
        loader.missing_calls[0],
        vec![alpha.path.clone(), beta.path.clone()]
    );
    assert_eq!(loader.cleanup_candidates, vec![vec![alpha.path, beta.path]]);
    assert!(term.frames.iter().any(|frame| {
        frame
            .join("\n")
            .contains("2 workspace registrations removed")
    }));
}

#[test]
fn open_filter_cleanup_confirmation_and_unite_selection_use_the_injected_loader() {
    let alpha = ws("alpha");
    let beta = ws("beta");

    let mut filter = FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('b'), Key::Quit]);
    run(
        &mut filter,
        vec![alpha.clone(), beta.clone()],
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(filter.frames[2].join("\n").contains("↳ /tmp/beta"));

    let mut cancel =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('C'), Key::Char('n'), Key::Quit]);
    let mut cancel_loader = FakeLoader::default();
    run(
        &mut cancel,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut cancel_loader,
    )
    .unwrap();
    assert_eq!(cancel_loader.cleanup_calls, 0);

    let mut confirm =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('C'), Key::Char('y'), Key::Quit]);
    let mut confirm_loader = FakeLoader {
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut confirm,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut confirm_loader,
    )
    .unwrap();
    assert_eq!(confirm_loader.cleanup_calls, 1);
    assert!(confirm.frames[3].join("\n").contains("No workspaces yet"));

    let mut unite = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Tab,
        Key::Char(' '),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut unite_loader = FakeLoader::default();
    run(
        &mut unite,
        vec![alpha, beta],
        Vec::new(),
        now(),
        &mut unite_loader,
    )
    .unwrap();
    assert_eq!(
        unite_loader.opened,
        vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")]
    );
}

#[test]
fn open_unregister_requires_confirmation_and_only_passes_the_selected_path_to_loader() {
    let alpha = ws("alpha");
    let beta = ws("beta");

    let mut cancel = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Down,
        Key::CtrlX,
        Key::Char('c'),
        Key::Quit,
    ]);
    let mut cancel_loader = FakeLoader::default();
    run(
        &mut cancel,
        vec![alpha.clone(), beta.clone()],
        Vec::new(),
        now(),
        &mut cancel_loader,
    )
    .unwrap();
    assert_eq!(cancel_loader.unregister_calls, 0);
    assert!(cancel.frames[3].join("\n").contains("Unregister workspace"));
    assert!(
        cancel.frames[3]
            .join("\n")
            .contains("Only the registry entry is removed. Files stay.")
    );
    assert!(cancel.frames[3].join("\n").contains("beta"));

    let mut confirm = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Down,
        Key::CtrlX,
        Key::Enter,
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    confirm.size = Some((24, 80));
    let mut confirm_loader = FakeLoader::default();
    run(
        &mut confirm,
        vec![alpha, beta.clone()],
        Vec::new(),
        now(),
        &mut confirm_loader,
    )
    .unwrap();
    assert_eq!(confirm_loader.unregister_calls, 1);
    assert_eq!(confirm_loader.unregistered, vec![beta.path]);
    assert!(
        confirm.frames[3]
            .join("\n")
            .contains("Unregister workspace")
    );
    assert!(confirm.frames[4].join("\n").contains("alpha"));
    assert!(!confirm.frames[4].join("\n").contains("beta"));
    let add = confirm
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .find(|frame| frame.contains("Add workspace"))
        .expect("the project add overlay opens");
    assert!(!add.contains("beta"));
}

#[test]
fn open_navigation_keeps_workspace_open_when_escape_is_pressed() {
    // Navigate the Open list to beta and open it, confirm Escape keeps the
    // workspace open, then detach through the controller quit chord.
    let keys = [
        Key::Char('o'),
        Key::Down,
        Key::Up,
        Key::Down,
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    run(
        &mut term,
        vec![ws("alpha"), ws("beta")],
        Vec::new(),
        now(),
        &mut loader,
    )
    .unwrap();
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/beta")]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("beta-session"))
    );
    assert!(term.frames.iter().any(|frame| {
        frame
            .join("\n")
            .contains("a: agent / t: terminal / Enter: actions")
    }));
}

#[test]
fn open_prev_wraps_and_escape_returns_to_welcome() {
    let keys = [
        Key::Char('o'),
        Key::Up,
        Key::Escape,
        Key::Char('q'),
        Key::Enter,
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        vec![ws("alpha"), ws("beta")],
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[1].join("\n").contains("alpha"));
    assert!(term.frames[2].join("\n").contains("beta"));
    assert!(term.frames[3].join("\n").contains("Menu"));
}

#[test]
fn open_list_keeps_registry_entries_the_recent_projection_does_not_carry() {
    // Welcome の recent カードは 3 枠だが、Open は登録済みを 1 件も落とさない。
    let registry = vec![ws("alpha"), ws("beta"), ws("gamma"), ws("delta")];
    // delta は単体 recent と Unite card の両方に現れる（production の `recent()` は常にこの形）。
    let projection = vec![
        Recent::Workspace(WorkspaceOverview::new(ws("delta"), 2, 3, 4)),
        Recent::Unite(UniteOverview::new(vec![
            WorkspaceOverview::new(ws("delta"), 2, 3, 4),
            WorkspaceOverview::new(ws("gamma"), 1, 1, 1),
        ])),
    ];

    let open = open_from_registry(registry, &projection);

    let names = open
        .workspaces()
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect::<Vec<_>>();
    // 両方に現れる path も 1 行だけになる。
    assert_eq!(names, ["alpha", "beta", "delta", "gamma"]);
    // 単体 recent も Unite card の member も集計値を保ち、どちらにも無い entry だけ 0 件で補われる。
    let rendered = render_open(24, 80, &open, now()).join("\n");
    assert!(rendered.contains("⎇ 2 sessions"));
    assert!(rendered.contains("⎇ 1 session"));
    assert!(rendered.contains("⎇ 0 sessions"));
}

#[test]
fn open_touch_keeps_workspace_open_when_escape_is_pressed() {
    let alpha = ws_minutes_ago("alpha", 20);
    let beta = ws_minutes_ago("beta", 10);
    let recent = vec![
        Recent::Workspace(WorkspaceOverview::new(beta.clone(), 2, 3, 4)),
        Recent::Workspace(WorkspaceOverview::new(alpha.clone(), 5, 6, 7)),
    ];
    let keys = [
        Key::Char('o'),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        opened_at: Some(now()),
        ..FakeLoader::default()
    };

    run(&mut term, vec![alpha, beta], recent, now(), &mut loader).unwrap();

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("alpha-session"))
    );
}

#[test]
fn empty_open_enter_stays_and_open_quit_exits() {
    let keys = [Key::Char('o'), Key::Enter, Key::Down, Key::Up, Key::Quit];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[1].join("\n").contains("No workspaces yet"));
    // Welcome and the empty Open list. Enter, Down and Up have nothing to
    // move in an empty list, so they draw nothing (#554).
    assert_eq!(term.frames.len(), 2);

    let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Tab, Key::Enter, Key::Quit]);
    run(
        &mut term,
        vec![ws("alpha")],
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    // Welcome, the Open list, and the Home frame the chosen workspace
    // opens. Tab completes onto the only entry and changes nothing.
    assert_eq!(term.frames.len(), 3);
}

#[test]
fn open_key_classifier_covers_edit_selection_and_confirmation_paths() {
    let mut open = Open::new(vec![ws("alpha"), ws("Alpha"), ws("beta")]);
    for key in [
        Key::Up,
        Key::Down,
        Key::Char('x'),
        Key::Backspace,
        Key::Left,
        Key::Right,
        Key::Home,
        Key::End,
        Key::LineStart,
        Key::LineEnd,
        Key::Delete,
        Key::SelectLeft,
        Key::SelectRight,
        Key::SelectHome,
        Key::SelectEnd,
        Key::Other,
    ] {
        assert!(matches!(step_open(&mut open, key), OpenStep::Stay));
    }
    assert!(matches!(
        step_open(&mut open, Key::Enter),
        OpenStep::Choose(_)
    ));
    assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Back));
    assert!(matches!(step_open(&mut open, Key::CtrlQ), OpenStep::Quit));

    let _ = step_open(&mut open, Key::Tab);
    let _ = step_open(&mut open, Key::Char(' '));
    let _ = step_open(&mut open, Key::Char(' '));
    let _ = step_open(&mut open, Key::Char(' '));
    assert!(matches!(
        step_open(&mut open, Key::Enter),
        OpenStep::Choose(_)
    ));

    let _ = step_open(&mut open, Key::Char('C'));
    assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Stay));
    let _ = step_open(&mut open, Key::Char('C'));
    assert!(matches!(
        step_open(&mut open, Key::Enter),
        OpenStep::ConfirmCleanup
    ));

    let _ = step_open(&mut open, Key::CtrlX);
    let _ = step_open(&mut open, Key::Left);
    assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Stay));
    let _ = step_open(&mut open, Key::CtrlX);
    assert!(matches!(
        step_open(&mut open, Key::Char('y')),
        OpenStep::ConfirmUnregister(_)
    ));

    for key in [Key::Right, Key::Tab, Key::Char('n'), Key::CtrlQ] {
        let mut open = Open::new(vec![ws("fresh")]);
        let _ = step_open(&mut open, Key::CtrlX);
        let result = step_open(&mut open, key.clone());
        assert!(matches!(result, OpenStep::Stay | OpenStep::Quit));
    }
    let mut open = Open::new(vec![ws("fresh")]);
    let _ = step_open(&mut open, Key::Char('C'));
    assert!(matches!(step_open(&mut open, Key::CtrlQ), OpenStep::Quit));
}

#[test]
fn recent_loads_workspace_and_escape_keeps_it_open() {
    let mut term =
        FakeTerminal::with_keys(&[Key::Char('1'), Key::Escape, Key::CtrlQ, Key::Char('y')]);
    let mut loader = FakeLoader::default();
    run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut loader,
    )
    .unwrap();
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/recent")]);
    assert!(term.frames[1].join("\n").contains("recent-session"));
    assert!(term.frames[2].join("\n").contains("recent-session"));
}

#[test]
fn recent_touch_keeps_workspace_open_when_escape_is_pressed() {
    let alpha = ws_minutes_ago("alpha", 20);
    let beta = ws_minutes_ago("beta", 10);
    let recent = vec![
        Recent::Workspace(WorkspaceOverview::new(beta.clone(), 2, 3, 4)),
        Recent::Workspace(WorkspaceOverview::new(alpha.clone(), 5, 6, 7)),
    ];
    let keys = [Key::Char('2'), Key::Escape, Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        opened_at: Some(now()),
        ..FakeLoader::default()
    };

    run(&mut term, vec![beta, alpha], recent, now(), &mut loader).unwrap();

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert!(term.frames[2].join("\n").contains("alpha-session"));
}

#[test]
fn unite_recent_reopens_the_ordered_workspace_deck() {
    let unite = Recent::Unite(UniteOverview::new(vec![
        WorkspaceOverview::new(ws("primary"), 0, 0, 0),
        WorkspaceOverview::new(ws("other"), 0, 0, 0),
    ]));
    let empty = Recent::Unite(UniteOverview::new(Vec::new()));
    let keys = [Key::Char('2'), Key::Char('1'), Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    run(
        &mut term,
        Vec::new(),
        vec![unite, empty],
        now(),
        &mut loader,
    )
    .unwrap();
    assert_eq!(
        loader.opened,
        vec![PathBuf::from("/tmp/primary"), PathBuf::from("/tmp/other")]
    );
    assert!(term.frames.iter().any(|frame| {
        let text = frame.join("\n");
        text.contains("primary") && text.contains("other")
    }));
}

#[test]
fn missing_recent_number_stays_on_welcome() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('3'), Key::Char('q'), Key::Enter]);
    run(
        &mut term,
        Vec::new(),
        vec![recent("only")],
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    // The out-of-range number leaves Welcome untouched, so it never redraws.
    assert_eq!(term.frames.len(), 1);
}

#[test]
fn screen_graph_injects_metrics_when_opening_a_workspace() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut sessions = SnapshotSessionPortFactory {
        calls,
        created: Arc::new(Mutex::new(0)),
    };
    let mut agents = IdleAgentPortFactory;
    let mut metrics = StaticMetricsFactory;
    // Open the workspace, then quit it through the controller's quit chord
    // (Ctrl-Q opens the confirmation, `y` detaches); `q` alone is inert now.
    let keys = [Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;

    assert_eq!(
        run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
            &mut term,
            vec![ws("alpha")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
            &mut agents,
            AvailableAgentModels::all(),
            &mut metrics,
        )
        .unwrap(),
        Exit::Quit
    );

    assert!(
        term.frames
            .iter()
            .flat_map(|frame| frame.iter())
            .any(|line| line.contains('\u{f2db}') && line.contains('\u{f233}'))
    );
}

#[test]
fn selecting_an_interrupted_rabbit_opens_the_same_unresumable_prompt() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let runtime_id = AgentRuntimeId::new();
    let continuation = history.continuation;
    let terminal = history.last_terminal.clone();
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        Box::new(UnavailablePaneLaunchPort),
    );
    ui.agent_inventory = Some(AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(runtime_id, terminal, Some(session)).unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Interrupted,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    });
    let mut pending_targets = std::collections::HashMap::new();
    let mut pointer_gesture = false;
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let record = SessionRecord {
        name: "interrupted".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/interrupted"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let material = home_frame_material(
        24,
        80,
        &runtime,
        "demo",
        &[ProjectedSession::from_record(session, &record)],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    )
    .with_agent_inventory(ui.agent_inventory(), runtime.panes());
    let click = (0..24)
        .flat_map(|row| (0..80).map(move |column| (column, row)))
        .find(|&(column, row)| {
            matches!(
                garden_click_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    column,
                    row,
                ),
                Some(GardenClick::Visit {
                    agent: Some(selected),
                    ..
                }) if selected == runtime_id
            )
        })
        .expect("the interrupted rabbit is clickable");

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            Some(&material),
            &Key::Click {
                column: click.0,
                row: click.1,
            },
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Agent(Vec::new())),
    );
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    assert_eq!(
        runtime
            .interrupted_removal_confirmation()
            .map(|prompt| prompt.tab().continuation),
        Some(continuation),
    );
    assert!(ui.pane_launches.is_empty());
}

/// `usagi open <path>` that could not reach a daemon lands in the switcher
/// instead of back on the shell, so the very first frame has to say why the
/// workspace it asked for is not on screen.
#[test]
fn a_notice_seeded_switcher_opens_with_the_reason_on_its_first_frame() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('q'), Key::Enter]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend_and_notice(
            &mut term,
            Vec::new(),
            vec![recent_at("first", now())],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
            Some("daemon unavailable: the daemon did not answer".to_owned()),
        )
        .unwrap(),
        Exit::Quit
    );

    let first = term.frames.first().expect("the switcher drew no frame");
    assert!(
        first
            .iter()
            .any(|line| line.contains("the daemon did not answer")),
        "the first frame must carry the reason: {first:?}"
    );
    // Nothing was opened: the notice explains an absence, it does not stand
    // in for a workspace that did load.
    assert!(loader.opened.is_empty());
}

/// #556 acceptance. Home can return to Welcome, another workspace opens from
/// there, and none of it needs a restarted process. All three entries —
/// Recent, Open, New — lead back to the switcher, and `Exit::Quit` stays the
/// process-exit answer reached only by choosing `quit`.
#[test]
fn every_workspace_entry_returns_to_welcome_without_restarting() {
    // Recent `1` (first) → leave → Open `o`/↓/Enter (second) → leave →
    // New Existing (`x`) → leave → quit from Welcome. Each `w` is the exit
    // prompt's leave answer.
    let mut keys = vec![Key::Char('1'), Key::CtrlQ, Key::Char('w')];
    keys.extend([
        Key::Char('o'),
        Key::Down,
        Key::Enter,
        Key::CtrlQ,
        Key::Char('w'),
    ]);
    keys.extend([
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::CtrlQ,
        Key::Char('w'),
    ]);
    // Back on Welcome for the third time, `q` ends the process.
    keys.push(Key::Char('q'));
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        opened_at: Some(now() + Duration::hours(1)),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
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
        .unwrap(),
        Exit::Quit
    );

    // Three distinct workspaces opened by the one process, in entry order,
    // each one rebinding the settings port to its own root. (`loader.opened`
    // records the New form's relative path; the snapshot resolves it.)
    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/first"),
            PathBuf::from("/tmp/second"),
            PathBuf::from("x"),
        ]
    );
    assert_eq!(
        settings.selected,
        vec![
            PathBuf::from("/tmp/first"),
            PathBuf::from("/tmp/second"),
            PathBuf::from("/tmp/x"),
        ]
    );
    assert_eq!(factory.drops_at_create.len(), 3);

    // Every departure landed on Welcome — the `Menu` heading belongs to the
    // switcher alone, so it is absent from the Open list and the New form
    // that were used to get there.
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    let mut from = 0;
    for workspace in ["first", "second", "x"] {
        let home = frames
            .iter()
            .skip(from)
            .position(|frame| frame.contains(workspace))
            .unwrap_or_else(|| panic!("{workspace} opens"))
            + from;
        let welcome = frames
            .iter()
            .skip(home + 1)
            .position(|frame| frame.contains("Menu"))
            .unwrap_or_else(|| panic!("leaving {workspace} draws Welcome"))
            + home
            + 1;
        assert!(
            !frames[welcome].contains("Open Workspace"),
            "leaving {workspace} must land on Welcome, not the Open list: {}",
            frames[welcome]
        );
        from = welcome;
    }
}

/// A workspace whose settings cannot be bound is a failure to report, not a
/// silent entry: the error propagates out of the screen graph instead of the
/// graph continuing with the previous workspace's settings. Every entry —
/// Recent, Open, New — propagates it the same way.
#[test]
fn a_settings_binding_failure_while_opening_a_workspace_propagates() {
    struct UnbindableSettings;

    impl SettingsPort for UnbindableSettings {
        fn select_workspace(&mut self, _workspace_root: &Path) -> io::Result<()> {
            Err(io::Error::other("settings directory is unavailable"))
        }

        fn read(
            &mut self,
            _scope: usagi_core::usecase::settings::SettingsScope,
        ) -> io::Result<Settings> {
            Ok(Settings::default())
        }

        fn save(
            &mut self,
            _scope: usagi_core::usecase::settings::SettingsScope,
            _settings: &Settings,
        ) -> io::Result<()> {
            Ok(())
        }
    }

    let cases = [
        (
            vec![Key::Char('1')],
            Vec::new(),
            vec![recent_at("first", now())],
        ),
        (
            vec![Key::Char('o'), Key::Enter],
            vec![ws("listed")],
            Vec::new(),
        ),
        (
            vec![
                Key::Char('e'),
                Key::Right,
                Key::Down,
                Key::Char('x'),
                Key::Enter,
            ],
            Vec::new(),
            Vec::new(),
        ),
    ];

    for (keys, workspaces, recent) in cases {
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut factory = CountingBackendFactory::new();

        let error = run_screen_graph_with_backend(
            &mut term,
            workspaces,
            recent,
            now(),
            Start::Welcome,
            &mut loader,
            &mut UnbindableSettings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "settings directory is unavailable");
        // The failure happened before any daemon port was created, so nothing
        // was established for a workspace that never opened.
        assert!(factory.drops_at_create.is_empty());
        assert_eq!(factory.drops.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn project_add_applies_a_registry_refresh_while_the_overlay_is_open() {
    let alpha = ws("alpha");
    let beta = ws("beta");
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        registry_refresh: Some(FakeRegistryRefresh::Queued(vec![alpha.clone(), beta])),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![alpha],
        Vec::new(),
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    assert_eq!(loader.registry_refresh_dispatches, 1);
    assert_eq!(
        loader.opened,
        vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")]
    );
}

#[test]
fn project_add_preserves_an_open_workspace_draft() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::Agent),
        Key::Paste("keep-me".to_owned()),
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::Escape,
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

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(factory.drops_at_create, vec![0]);
    assert!(
        term.frames.iter().any(|frame| {
            contains_wrapped(
                &frame.join("\n"),
                "Save or cancel the current draft before switching.",
            )
        }),
        "{:#?}",
        term.frames
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("keep-me"))
    );
}

/// #556 acceptance: the workspace fence still refuses, and it refuses as a
/// notice on the screen the user is standing on — including the Welcome that
/// was reached by leaving a workspace. No silent fallback to the workspace
/// that was left.
#[test]
fn a_fenced_workspace_refuses_on_the_welcome_reached_by_leaving() {
    const REFUSAL: &str = "cannot open /tmp/second: this daemon does not serve the selected \
             workspace; this daemon serves the workspace /tmp/first.";

    let mut term = FakeTerminal::with_keys(&[
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('w'),
        // The fenced workspace keeps Welcome up; the served one still opens.
        Key::Char('2'),
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        refuse_paths: vec![PathBuf::from("/tmp/second")],
        opened_at: Some(now() + Duration::hours(1)),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
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
        .unwrap(),
        Exit::Quit
    );

    // The refused open was attempted and reported; only the served workspace
    // ever became a composition.
    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/first"),
            PathBuf::from("/tmp/second"),
            PathBuf::from("/tmp/first"),
        ]
    );
    assert_eq!(factory.drops_at_create.len(), 2);
    let welcome = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .rev()
        .find(|frame| frame.contains("Menu"))
        .expect("the switcher stays on screen after the refusal");
    assert!(contains_wrapped(&welcome, REFUSAL), "{welcome}");
}

/// #556 acceptance: the splash is skippable. A key press during it ends the
/// animation at that frame instead of holding the terminal for its full
/// 14-frame run.
#[test]
fn a_key_press_skips_the_rest_of_the_startup_splash() {
    let frames = crate::presentation::views::splash::FRAMES;
    // No input at all: every frame plays, as before.
    let mut full = SplashTerminal::new(vec![None; frames]);
    assert_eq!(play_startup_splash(&mut full).unwrap(), frames);
    assert_eq!(full.frames.len(), frames);

    // A key on the third frame ends it there.
    let mut skipped = SplashTerminal::new(vec![None, None, Some(Key::Char('o'))]);
    assert_eq!(play_startup_splash(&mut skipped).unwrap(), 3);
    assert_eq!(skipped.frames.len(), 3);

    // A wake-up tick and a resize are not key presses: they keep the pace,
    // and the next frame re-reads the terminal size.
    let mut paced = SplashTerminal::new(
        std::iter::repeat_n(Some(Key::Other), frames / 2)
            .chain(std::iter::repeat_n(Some(Key::Resize), frames - frames / 2))
            .collect(),
    );
    assert_eq!(play_startup_splash(&mut paced).unwrap(), frames);
    assert_eq!(paced.frames.len(), frames);
}

/// #556 acceptance: the splash belongs to launching the process, not to
/// arriving at Welcome. The Welcome reached by leaving a workspace draws no
/// splash frame at all.
#[test]
fn the_startup_splash_plays_once_per_process() {
    let mut splash = crate::presentation::StartupSplash::new();
    let frames = crate::presentation::views::splash::FRAMES;

    let mut term = SplashTerminal::new(vec![None; frames]);
    assert_eq!(splash.play(&mut term).unwrap(), frames);
    assert_eq!(term.frames.len(), frames);

    // Returning to Welcome is not a launch: no size read, no draw, no wait.
    let mut again = SplashTerminal::new(Vec::new());
    assert_eq!(splash.play(&mut again).unwrap(), 0);
    assert!(again.frames.is_empty());
}

/// #556 acceptance: an interrupted splash still hands Welcome its correct
/// initial state. The skip key is consumed by the skip, so Welcome starts on
/// its own first frame and reads its own first input.
#[test]
fn welcome_starts_correctly_after_an_interrupted_splash() {
    let mut term = SplashTerminal::new(vec![Some(Key::Char('o'))]).with_keys(&[
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut splash = crate::presentation::StartupSplash::new();
    let played = splash.play(&mut term).unwrap();
    assert_eq!(played, 1);

    let mut loader = FakeLoader::default();
    assert_eq!(
        run_from_start(
            &mut term,
            Vec::new(),
            vec![recent_at("first", now())],
            now(),
            Start::Welcome,
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );

    // The frame right after the skipped splash is the switcher, drawn from
    // the given Recent; Welcome's own keys then drive it as usual.
    let welcome = term.frames[played].join("\n");
    assert!(welcome.contains("Menu"), "{welcome}");
    assert!(welcome.contains("first"), "{welcome}");
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/first")]);
}

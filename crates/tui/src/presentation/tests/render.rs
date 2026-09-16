//! render の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn failed_delete_selection_renders_the_force_remove_confirmation() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let record = SessionRecord {
        name: "feature".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/feature"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let mut projected = ProjectedSession::from_record(session, &record);
    projected.lifecycle = SessionLifecycle::Failed;
    projected.failure_stage = Some(usagi_core::domain::session_lifecycle::FailureStage::Delete);
    projected.failure_summary = Some("safe detail".to_owned());
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
        BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Delete),
                failure_summary: Some("safe detail".to_owned()),
            },
        )]),
    )));
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));

    let material = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &[projected],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    let frame = render_home_material(&material).join("\n");

    assert!(frame.contains("Force remove"));
    assert!(frame.contains("Force remove feature?"));
    assert!(frame.contains("[ yes ]"));
    assert!(frame.contains("[ no  ]"));
    assert!(frame.contains("Previous removal failed. Changes may be discarded."));
}

#[test]
fn render_controller_frame_composites_the_home_and_overlays() {
    use crate::presentation::views::workspace::ProjectedSession;
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, Effect, Notice, OperationResult,
    };

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let projected = ProjectedSession {
        branch: "usagi/alpha".into(),
        id: session,
        label: "alpha".into(),
        detail: "fixture".into(),
        cwd: "/work/alpha".into(),
        last_modified: now(),
        has_notes: false,
        pr_count: 0,
        removing: false,
        agent_resume: None,
        lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
        failure_stage: None,
        failure_summary: None,
        role_id: None,
        parent_session_id: None,
        organization_depth: 0,
    };
    let sessions = std::slice::from_ref(&projected);
    let git = std::collections::BTreeMap::new();
    // Every case here composites the same Home geometry; only the runtime
    // and its session rows vary. Diagnostic health uses its unobserved
    // default so these assertions stay about the overlays.
    let frame = |runtime: &WorkspaceRuntime, sessions: &[ProjectedSession]| {
        render_controller_frame(
            20,
            80,
            runtime,
            "atlas",
            sessions,
            None,
            health(),
            &git,
            None,
            None,
        )
    };

    // Base Home frame: project identity stays in the outer tab bar, while
    // the Home frame renders its session row without a duplicate breadcrumb.
    let runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let base = frame(&runtime, sessions);
    assert!(!base.join("\n").contains("atlas"));
    assert!(base.join("\n").contains("alpha"));

    // Create form: with no sessions a single Down reaches + new session. It
    // renders inline in the sidebar row (the typed name), not as a centered
    // "New session" modal.
    let mut creating = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = creating.handle_key(Key::Down);
    let _ = creating.handle_key(Key::Enter);
    for character in ['b', 'e', 't', 'a'] {
        let _ = creating.handle_key(Key::Char(character));
    }
    let create = frame(&creating, &[]);
    assert!(create.join("\n").contains("beta"));
    assert!(!create.join("\n").contains("New session"));

    // Exit prompt overlay: the shared choice buttons and shortcut lines
    // render, defaulting to `quit` focused.
    let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = quitting.apply_event(AppEvent::Key(AppKey::CtrlQ));
    let quit = frame(&quitting, sessions);
    let quit_text = quit.join("\n");
    assert!(quit_text.contains("Leave this workspace?"));
    assert!(quit_text.contains("[ welcome ]"));
    assert!(quit_text.contains("[ quit    ]"));
    assert!(quit_text.contains("[ stay    ]"));
    assert!(quit_text.contains("←→/Tab: move"));

    // The runtime's persisted Overview palette renders through this path.
    let mut palette = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = palette.handle_key(Key::Char(':'));
    let overview = frame(&palette, sessions);
    assert!(overview.join("\n").contains("Overview"));

    let _ = palette.apply_event(AppEvent::Key(AppKey::SubmitOverview(
        "roles workspace".to_owned(),
    )));
    let _ = palette.apply_event(AppEvent::Backend(BackendEvent::RolesLoaded {
        scope: RoleEditorScope::Workspace,
        source: "version = 1\n".to_owned(),
    }));
    let roles = frame(&palette, sessions);
    assert!(roles.join("\n").contains("workspace roles.toml"));

    // Create-failure dialog: a failed create OperationResult opens it, and
    // this path composites the safe message over Home.
    let mut failing = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = failing.handle_key(Key::Down);
    let _ = failing.handle_key(Key::Enter);
    for character in ['a', 'p', 'i'] {
        let _ = failing.handle_key(Key::Char(character));
    }
    let token = match &failing.handle_key(Key::Enter)[..] {
        [Effect::CreateSession { token, .. }] => *token,
        other => panic!("expected a create effect, got {other:?}"),
    };
    let _ = failing.apply_event(AppEvent::OperationResult(OperationResult {
        token,
        succeeded: false,
        created: None,
        notice: Some(Notice::new("worktree path already exists")),
    }));
    let failure = frame(&failing, &[]);
    assert!(failure.join("\n").contains("Session create failed"));
    assert!(failure.join("\n").contains("worktree path already exists"));
}

#[test]
fn render_controller_frame_composites_agent_launch_failure() {
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{AppEvent, Notice};

    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.apply_event(AppEvent::AgentLaunchFailed(Notice::new(
        "agent process could not be started",
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
    assert!(failure.contains("Agent failed to start"));
    assert!(failure.contains("agent process could not be started"));
}

#[test]
fn render_controller_frame_draws_a_waving_pending_create_skeleton() {
    // Once a create request is in flight, the shell threads its name here and
    // the sidebar draws a three-line loading skeleton just above `+ new
    // session` (document/03-tui.md). The sweep paints each cell with its own
    // SGR run, so compare on ANSI-stripped text.
    let strip = |frame: &[String]| {
        frame
            .iter()
            .map(|line| {
                let mut out = String::new();
                let mut chars = line.chars();
                while let Some(ch) = chars.next() {
                    if ch == '\u{1b}' {
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                                break;
                            }
                        }
                    } else {
                        out.push(ch);
                    }
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let workspace = WorkspaceId::new();
    let git = std::collections::BTreeMap::new();

    let idle = WorkspaceRuntime::new(workspace, Vec::new());
    let pending = render_controller_frame(
        20,
        80,
        &idle,
        "atlas",
        &[],
        None,
        health(),
        &git,
        None,
        Some("beta"),
    );
    let pending_text = strip(&pending);
    assert!(pending_text.contains("+ beta"));
    assert!(pending_text.contains("creating"));

    // No pending create means no skeleton or loading caption.
    let quiet = render_controller_frame(
        20,
        80,
        &idle,
        "atlas",
        &[],
        None,
        health(),
        &git,
        None,
        None,
    );
    let quiet_text = strip(&quiet);
    assert!(!quiet_text.contains("beta"));
    assert!(!quiet_text.contains("creating"));

    // The wave advances with the mascot tick rather than blinking statically.
    let mut ticked = WorkspaceRuntime::new(workspace, Vec::new());
    for _ in 0..12 {
        let _ = ticked.apply_event(AppEvent::Tick);
    }
    let pending_ticked = render_controller_frame(
        20,
        80,
        &ticked,
        "atlas",
        &[],
        None,
        health(),
        &git,
        None,
        Some("beta"),
    );
    assert_ne!(pending, pending_ticked);
}

#[test]
fn controller_loop_renders_home_and_detaches_on_quit_confirmation() {
    let snapshot = snapshot("demo");
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: snapshot.workspace_id,
        session_id: snapshot.session_ids.first().copied(),
        worktree_id: WorktreeId::new(),
    };
    // Ctrl-Q opens the quit confirmation; `y` detaches and ends the loop.
    let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, Key::Char('y')]);
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
    // The controller Home frame renders through render_home, and the quit
    // confirmation is composited before the loop detaches.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("demo"))
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Leave this workspace?"))
    );
    // Regression: the real Ctrl-Q frame carries the shared choice buttons and
    // the ←→/Tab shortcut, not the old free-text y/n prompt. Leaving and
    // quitting are separate buttons (#556).
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("[ quit    ]")),
        "exit prompt frame is missing the [ quit ] button"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("[ welcome ]")),
        "exit prompt frame is missing the [ welcome ] button"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("[ stay    ]")),
        "exit prompt frame is missing the [ stay ] button"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("←→/Tab: move")),
        "exit prompt frame is missing the move shortcut"
    );
}

/// #554 acceptance. Skipping is decided by comparing the renderer's inputs,
/// so this pins each of those inputs: change one and the frame must differ,
/// change none and it must not — including across the ticks the rabbit
/// spends resting.
#[test]
#[allow(clippy::too_many_lines)] // One arm per material the renderer reads; splitting hides the table.
fn the_frame_material_changes_for_every_input_the_renderer_reads() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let record = SessionRecord {
        name: "alpha".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/alpha"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let sessions = vec![ProjectedSession::from_record(session, &record)];
    let no_diffs = BTreeMap::new();
    let clock = crate::presentation::relative_time_clock(now()) + Duration::seconds(10);
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

    let material = |runtime: &WorkspaceRuntime| {
        home_frame_material(
            20,
            80,
            runtime,
            "demo",
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            clock,
        )
    };
    let base = material(&runtime);

    // The decorative rabbit holds its resting pose for 32 runtime ticks, so
    // an idle Home does not repaint at the terminal clock cadence.
    for _ in 0..31 {
        let _ = runtime.apply_event(AppEvent::Tick);
        assert_eq!(material(&runtime), base, "a resting tick forced a redraw");
    }
    // The blink and the ear flop are each held for eight ticks, but both
    // transitions must still reach the terminal.
    let _ = runtime.apply_event(AppEvent::Tick);
    let blink = material(&runtime);
    assert_ne!(blink, base, "the rabbit stopped blinking");
    for _ in 0..8 {
        let _ = runtime.apply_event(AppEvent::Tick);
    }
    let flop = material(&runtime);
    assert_ne!(flop, blink, "the rabbit stopped flopping its ear");
    for _ in 0..8 {
        let _ = runtime.apply_event(AppEvent::Tick);
    }
    assert_eq!(
        material(&runtime),
        base,
        "the rabbit never came back to rest"
    );

    // Terminal size (a resize that actually changes the geometry).
    let resized = home_frame_material(
        21,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock,
    );
    assert_ne!(resized, base, "a resize did not redraw");

    // The wall clock behind relative session times is independent from the
    // monotonic animation clock. Neither sub-second nor one-second changes
    // rebuild an ordinary Home; the next minute does.
    let sub_second = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock + Duration::milliseconds(400),
    );
    assert_eq!(sub_second, base, "sub-second jitter forced a redraw");
    let next_second = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock + Duration::seconds(1),
    );
    assert_eq!(next_second, base, "a wall-clock second forced a redraw");
    let next_minute = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock + Duration::minutes(1),
    );
    assert_ne!(next_minute, base, "the relative session times froze");

    // Daemon metrics for the mascot sidecar.
    let metrics = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        StaticMetrics.latest(),
        health(),
        &no_diffs,
        None,
        None,
        clock,
    );
    assert_ne!(metrics, base, "a metrics update did not redraw");

    // The diagnostic health observer. It is a renderer input, so it belongs
    // to the material: a newly observed sample must be able to change the
    // sidecar, and an unchanged observer must not force a redraw.
    let mut observed = DaemonHealthTracker::default();
    observed.observe(&StaticMetrics.latest().expect("static metrics"));
    let health_material = home_frame_material(
        20, 80, &runtime, "demo", &sessions, None, observed, &no_diffs, None, None, clock,
    );
    assert_ne!(health_material, base, "a health observation did not redraw");

    // Git diffs joined onto the sidebar rows.
    let diffs = BTreeMap::from([(
        session,
        GitDiff {
            base: "main".to_owned(),
            ahead: 1,
            behind: 0,
            added: 1,
            removed: 2,
        },
    )]);
    let git = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &diffs,
        None,
        None,
        clock,
    );
    assert_ne!(git, base, "a git diff update did not redraw");

    // Live terminal output.
    let view = TerminalViewProjection {
        rows: vec!["output".to_owned()],
        row_offset: 0,
        total_rows: 1,
        scroll: 0,
        feedback: None,
    };
    let terminal_output = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        Some(view.clone()),
        None,
        clock,
    );
    assert_ne!(terminal_output, base, "terminal output did not redraw");

    let mut root_drawer = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = root_drawer.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let root_output = home_frame_material(
        20,
        80,
        &root_drawer,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        Some(view),
        None,
        clock,
    );
    assert!(root_output.projection.terminal_view().is_none());
    assert_eq!(
        render_home_material(&root_output)
            .join("\n")
            .matches("output")
            .count(),
        1
    );

    // The pending create skeleton.
    let pending = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        Some("beta"),
        clock,
    );
    assert_ne!(pending, base, "a pending create did not redraw");

    // Reducer state, and the two overlays composited outside `render_home`.
    let mut moved = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = moved.handle_key(Key::Down);
    assert_ne!(material(&moved), base, "a selection move did not redraw");

    let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = quitting.handle_key(Key::CtrlQ);
    let confirming = material(&quitting);
    assert_ne!(confirming, base, "the quit confirmation did not redraw");
    let _ = quitting.handle_key(Key::Left);
    assert_ne!(
        material(&quitting),
        confirming,
        "moving the quit confirmation's focus did not redraw"
    );
}

#[test]
fn background_exits_are_applied_at_a_bounded_rate_per_frame() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
    let exited = Arc::new(Mutex::new(Vec::new()));
    let first = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        first.clone(),
        Box::new(BackgroundLanePort {
            log: Arc::clone(&log),
            exited: Arc::clone(&exited),
        }),
    );
    let mut background = vec![first];
    for _ in 0..MAX_BACKGROUND_EXITS_PER_FRAME + 3 {
        let terminal = live_terminal_ref(workspace, session);
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
        let _ = runtime.complete_pane(Target::Session(session), operation, terminal.clone());
        background.push(terminal);
    }
    let foreground = background.pop().expect("the last tab stays selected");
    let _ = runtime.focus_terminal(Target::Session(session), foreground.clone());
    ui.sync_foreground_terminal(Some(&foreground), terminal_geometry(20, 80));
    exited.lock().unwrap().extend(background.iter().cloned());

    close_exited_panes(&mut ui, &mut runtime);
    assert_eq!(
        runtime.active_pane().tabs().len(),
        background.len() + 1 - MAX_BACKGROUND_EXITS_PER_FRAME,
        "one frame applies at most the bounded slice of background exits"
    );
    // The remainder lands on the following frames, none of it lost.
    close_exited_panes(&mut ui, &mut runtime);
    close_exited_panes(&mut ui, &mut runtime);
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert!(runtime.state().has_live_pane());
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers every wheel route with shared pane geometry.
fn physical_wheel_follows_full_screen_program_input_modes() {
    let cases = [
        (
            b"\x1b[?1000h\x1b[?1006hclaude".as_slice(),
            Some(b"\x1b[<64;5;1M".repeat(3)),
        ),
        (
            b"\x1b[?1049h\x1b[?1hcodex".as_slice(),
            Some(b"\x1bOA".repeat(crate::presentation::WHEEL_LINES * 3)),
        ),
        (b"\x1b[?1000hclaude".as_slice(), None),
    ];

    for (replay, expected) in cases {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(WheelRecordingPort {
                terminal,
                replay: replay.to_vec(),
                inputs: Arc::clone(&inputs),
                input_error: expected.is_none(),
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let geometry = terminal_geometry(20, 80);
        let (_, rows_len, scroll) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut pending = std::collections::HashMap::new();

        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: 41,
                row: 5,
                notches: 3,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
        assert_eq!(inputs.lock().unwrap().as_slice(), expected.as_slice());
        if expected.is_none() {
            assert!(controls.project(Vec::new(), 1).feedback.is_some());
        }
    }

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let mut replay = String::new();
    for row in 0..30 {
        use std::fmt::Write as _;
        let _ = writeln!(replay, "row {row}\r");
    }
    let replay = replay.into_bytes();
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(WheelRecordingPort {
            terminal: terminal.clone(),
            replay,
            inputs: Arc::clone(&inputs),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let geometry = terminal_geometry(20, 80);
    let (_, rows_len, scroll) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 0,
            row: 0,
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
        rows_len,
        scroll,
    ));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 41,
            row: 5,
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
        rows_len,
        scroll,
    ));
    let (view, _, _) = poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    assert_eq!(
        view.expect("primary history").scroll,
        crate::presentation::WHEEL_LINES
    );
    assert!(inputs.lock().unwrap().is_empty());

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: false,
            column: 41,
            row: 5,
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
        rows_len,
        scroll,
    ));
    let (view, _, _) = poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    assert_eq!(view.expect("primary history").scroll, 0);

    ui.close_terminal(&terminal);
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 41,
            row: 5,
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
        rows_len,
        scroll,
    ));

    let empty_view = WorkspaceView::with_runtime_ids(ws("empty"), empty_state("empty"), vec![]);
    let mut empty_ui = WorkspaceIoRuntime::new(empty_view, Box::new(UnavailableSessionCommandPort));
    let mut empty_runtime = WorkspaceRuntime::new(WorkspaceId::new(), vec![]);
    let _ = empty_runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let drawer = crate::presentation::director_drawer::geometry(20, 80);
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: u16::try_from(drawer.left.saturating_add(2)).expect("drawer column"),
            row: u16::try_from(drawer.top.saturating_add(4)).expect("drawer row"),
            notches: 1,
        }),
        &mut empty_ui,
        &mut empty_runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        rows_len,
        scroll,
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // This regression keeps the visible stale ref and latest lineage together.
fn visible_old_ref_can_close_latest_lineage_while_fresh_observation_is_pending() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let old = scoped_terminal_ref(workspace, Some(session));
    let replacement = scoped_terminal_ref(workspace, Some(session));
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: old.clone(),
        select: true,
    });
    initial.revision = 1;
    let durable = Arc::new(Mutex::new(initial));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let inventory = |terminal: &TerminalRef| AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    let terminals = |terminal: &TerminalRef| {
        vec![TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        }]
    };
    let completion =
        |terminal: &TerminalRef, fence: (u64, u64), port: Box<dyn AgentCommandPort>| {
            crate::presentation::RestoreCompletion {
                port,
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(terminals(terminal)),
                agents: Ok(inventory(terminal)),
                observation_coherent: true,
            }
        };

    let first_fence = runtime.restore_fence();
    let first = crate::presentation::apply_restore_completion(
        completion(&old, first_fence, Box::new(UnavailableAgentCommandPort)),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        first.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert_eq!(runtime.focused_terminal(), Some(old.clone()));

    // Another TUI advances this continuation from O to R. The late O
    // observation updates local durable state but must leave the visible O
    // pane untouched until its immediately scheduled fresh observation.
    {
        let mut latest = durable.lock().unwrap();
        latest.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: replacement.clone(),
            select: true,
        });
        latest.revision += 1;
    }
    let stale_fence = runtime.restore_fence();
    let stale = crate::presentation::apply_restore_completion(
        completion(&old, stale_fence, first.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        stale.outcome,
        crate::presentation::RestoreJobOutcome::FenceRejected
    );
    assert_eq!(runtime.focused_terminal(), Some(old.clone()));
    assert_eq!(ui.agent_continuation_for(&old), Some(continuation));

    crate::presentation::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.focused_terminal(), Some(old));
    assert!(durable.lock().unwrap().dismissed.is_empty());
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&replacement)
    );

    let fresh_fence = runtime.restore_fence();
    let fresh = crate::presentation::apply_restore_completion(
        completion(&replacement, fresh_fence, stale.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        fresh.outcome,
        crate::presentation::RestoreJobOutcome::Applied
    );
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(runtime.focused_terminal(), Some(replacement));
}

#[test]
fn closing_selected_agent_keeps_it_visible_without_focus_drift() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = AgentContinuationRef::new();
    let closed = AgentContinuationRef::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let closed_terminal = scoped_terminal_ref(workspace, Some(session));
    let generic = scoped_terminal_ref(workspace, Some(session));
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: first,
        terminal: first_terminal.clone(),
        select: false,
    });
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: closed,
        terminal: closed_terminal.clone(),
        select: true,
    });
    let durable = Arc::new(Mutex::new(intent));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(WheelRecordingPort {
                terminal: generic.clone(),
                replay: Vec::new(),
                inputs: Arc::new(Mutex::new(Vec::new())),
                input_error: false,
            }),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first_terminal,
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: closed_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: generic.clone(),
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(generic.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let _ = runtime.focus_terminal(
        Target::Session(session),
        durable.lock().unwrap().targets[0].tabs[1].terminal.clone(),
    );

    crate::presentation::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert_eq!(runtime.focused_terminal(), Some(closed_terminal));
    {
        let state = durable.lock().unwrap();
        assert!(state.dismissed.is_empty());
        assert_eq!(state.targets[0].selected, Some(closed));
    }

    // Closing a generic tab is not a durable conversation dismissal. It
    // records only a process-local exact fence against inventory restore.
    let _ = runtime.focus_terminal(Target::Session(session), generic.clone());
    ui.start_terminal_session(generic.clone(), terminal_geometry(20, 80));
    let before = durable.lock().unwrap().clone();
    crate::presentation::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(!runtime.active_pane().tabs().iter().any(|tab| matches!(
        tab,
        PaneTab::Live(LivePane { terminal, .. }) if terminal.fences(&generic)
    )));
    assert_eq!(*durable.lock().unwrap(), before);
    assert_eq!(ui.closed_generic_terminals, BTreeSet::from([generic]));
}

#[test]
fn non_cancellable_loading_throttles_frames_without_consuming_input() {
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::Enter]),
        ..ResponsiveLoadingTerminal::default()
    };
    let draw_count = Arc::clone(&term.draw_count);

    run_workspace_loading(&mut term, "Saving settings…", false, || {
        while draw_count.load(std::sync::atomic::Ordering::Acquire) < 2 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    })
    .expect("non-cancellable save completes");

    assert!(
        term.frames.len() >= 2,
        "the loading surface kept repainting"
    );
    assert!(
        !term.waits.is_empty(),
        "the render loop yielded between frames"
    );
    assert!(
        term.waits
            .iter()
            .all(|duration| *duration == std::time::Duration::from_millis(80))
    );
    assert_eq!(term.wait_keys, VecDeque::from([Key::Enter]));
}

#[test]
fn render_home_snapshot_draws_the_initial_home_surface() {
    // The non-interactive `usagi open <path>` fallback renders one static
    // project bar plus Home frame through the controller projection: the
    // workspace name, its sessions, and both creation affordances.
    let rows = render_home_snapshot(30, 100, &snapshot("demo"), IconMode::NerdFont);
    assert_eq!(rows.len(), 30);
    assert!(rows[0].contains("1 demo"));
    assert!(rows[0].contains("+ Open"));
    let frame = rows.join("\n");
    assert!(frame.contains("demo"));
    assert!(frame.contains("demo-session"));
    assert!(frame.contains("+ new session"));
    // A zero size safely falls back to the default geometry.
    assert!(!render_home_snapshot(0, 0, &snapshot("demo"), IconMode::NerdFont).is_empty());

    let text =
        strip_ansi(&render_home_snapshot(30, 100, &snapshot("demo"), IconMode::Text).join("\n"));
    for nerd_font_glyph in ["\u{f0ec}", "\u{f00e}", "\u{f085}"] {
        assert!(!text.contains(nerd_font_glyph));
    }
    assert!(text.contains("switch  closeup"));
    assert!(text.contains("Agents"));

    // A Failed session in the snapshot renders with its failed treatment and
    // failure reason, so the initial fallback frame surfaces it too.
    let mut failed_snapshot = snapshot("demo");
    let id = failed_snapshot.session_ids[0];
    failed_snapshot.session_lifecycles.insert(
        id,
        usagi_core::domain::session_lifecycle::SessionLifecycleProjection {
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Failed,
            failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Create),
            failure_summary: Some("branch exists".into()),
        },
    );
    let failed_frame =
        render_home_snapshot(30, 100, &failed_snapshot, IconMode::NerdFont).join("\n");
    assert!(failed_frame.contains("failed"));
    assert!(failed_frame.contains("branch exists"));
}

#[test]
fn write_banner_writes_description_line() {
    let mut buf = Vec::new();
    write_banner(&mut buf, &info()).unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "usagi v0.1.0\n");
}

#[test]
fn banner_screen_runner_names_non_interactive_tui_screens() {
    let entries = [
        EntryScreen::Welcome,
        EntryScreen::Workspace {
            path: PathBuf::from("/tmp/project"),
        },
        EntryScreen::Config,
    ];
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::new(&mut buf, &info);
    for entry in &entries {
        dispatch(entry, &mut runner).unwrap();
    }
    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "usagi v0.1.0: welcome TUI\n\
             usagi v0.1.0: workspace TUI (/tmp/project)\n\
             usagi v0.1.0: config TUI\n"
    );
}

#[test]
fn doctor_runner_renders_checks_and_summary() {
    use crate::usecase::doctor::{CheckStatus, DiagnosticCheck, DoctorReport};

    let report = DoctorReport {
        checks: vec![
            DiagnosticCheck {
                name: "Git",
                status: CheckStatus::Pass,
                detail: "git version 2.50".to_owned(),
            },
            DiagnosticCheck {
                name: "Codex CLI",
                status: CheckStatus::Warning,
                detail: "not found".to_owned(),
            },
            DiagnosticCheck {
                name: "Daemon",
                status: CheckStatus::Fail,
                detail: "connection refused".to_owned(),
            },
        ],
    };
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::with_doctor_report(&mut buf, &info, &report);
    dispatch(&EntryScreen::Doctor, &mut runner).unwrap();

    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "usagi v0.1.0: doctor\n\
             [ok] Git: git version 2.50\n\
             [warn] Codex CLI: not found\n\
             [error] Daemon: connection refused\n\
             result: problems found\n"
    );
}

#[test]
fn doctor_runner_renders_a_healthy_summary() {
    use crate::usecase::doctor::DoctorReport;

    let report = DoctorReport { checks: Vec::new() };
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::with_doctor_report(&mut buf, &info, &report);
    dispatch(&EntryScreen::Doctor, &mut runner).unwrap();
    assert!(
        String::from_utf8(buf)
            .unwrap()
            .ends_with("result: healthy\n")
    );
}

#[test]
fn banner_screen_runner_propagates_write_failure() {
    let mut out = FailingWriter;
    out.flush().unwrap();
    let info = info();
    let mut runner = BannerScreenRunner::new(&mut out, &info);
    assert_eq!(
        dispatch(&EntryScreen::Welcome, &mut runner)
            .unwrap_err()
            .to_string(),
        "write failed"
    );
}

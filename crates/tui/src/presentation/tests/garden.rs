//! garden の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn garden_claims_shell_owned_terminal_input_as_wake_events() {
    assert!(garden_shell_owned_wake(&Key::Pointer(PointerEvent {
        kind: crate::usecase::terminal_input::PointerKind::Drag,
        column: 1,
        row: 1,
    })));
    assert!(garden_shell_owned_wake(&Key::Live(
        LiveTerminalAction::ScrollDown
    )));
    assert!(garden_shell_owned_wake(&Key::Passthrough(vec![1])));
    assert!(garden_shell_owned_wake(&Key::Enter));
    assert!(garden_shell_owned_wake(&Key::Quit));
    assert!(garden_shell_owned_wake(&Key::CtrlQ));
    assert!(!garden_shell_owned_wake(&Key::Other));
}

/// The whole automatic path, minus the single thing that is genuinely the
/// OS's — the monotonic clock the shell injects. Frame wake-ups accumulate
/// idle time, an interaction throws it away, the threshold opens the
/// overlay, the very next frame *is* the garden, and a click resolved
/// against that frame's own plots lands in the rabbit's Closeup.
#[test]
#[allow(clippy::too_many_lines)] // End-to-end screen-saver timing and click ownership stay in one scenario.
fn an_idle_home_grows_a_garden_whose_usagi_is_one_click_from_its_closeup() {
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
    let clock = now();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let material = |runtime: &WorkspaceRuntime| {
        home_frame_material(
            24,
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

    // The composition root's frame period. Idle time is measured in these,
    // and driving them one by one is what proves the threshold is reached
    // rather than assumed.
    let frame = std::time::Duration::from_millis(16);
    let mut watch = IdleWatch::new(std::time::Duration::ZERO);
    let mut elapsed = std::time::Duration::ZERO;
    let wake = |runtime: &mut WorkspaceRuntime,
                watch: &mut IdleWatch,
                elapsed: &mut std::time::Duration,
                key: &Key| {
        *elapsed += frame;
        let idle = watch.observe(key, *elapsed);
        let _ = runtime.apply_event(AppEvent::IdleElapsed(idle));
    };

    // Four minutes of ticks — an Agent streaming output the whole time —
    // and Home is still Home.
    let four_minutes = GARDEN_IDLE_THRESHOLD
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("the threshold is longer than a minute");
    while elapsed < four_minutes {
        wake(&mut runtime, &mut watch, &mut elapsed, &Key::Other);
    }
    assert_eq!(runtime.state().overlay(), None);

    // One keypress and the four minutes are gone.
    wake(&mut runtime, &mut watch, &mut elapsed, &Key::Down);
    let restarted_at = elapsed;
    while elapsed < restarted_at + GARDEN_IDLE_THRESHOLD {
        assert_eq!(
            runtime.state().overlay(),
            None,
            "the garden opened {:?} after the last interaction",
            elapsed.saturating_sub(restarted_at)
        );
        wake(&mut runtime, &mut watch, &mut elapsed, &Key::Other);
    }
    assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));

    // The next frame is the garden itself, drawn full width over Home.
    let garden = material(&runtime);
    let rows = render_home_material(&garden);
    let text = rows.join("\n");
    assert_eq!(rows.len(), 24);
    assert!(
        text.contains("Garden Action Center · click a usagi") && text.contains("any key · wake")
    );
    assert!(text.contains("alpha"));

    // Every cell of the frame resolves through the same layout that drew it,
    // so the rabbit is reachable by clicking where it is drawn and the rest
    // of the garden is a wake-up.
    let resolve = |column: u16, row: u16| {
        garden_click_at(
            garden.height,
            garden.width,
            &garden.projection,
            garden.now,
            column,
            row,
        )
    };
    // Agent の居ない session なので、押せるのは区画（agent 無しの訪問）である。
    let visit = Some(GardenClick::Visit {
        workspace,
        session,
        agent: None,
    });
    let plot = (0..24)
        .flat_map(|row| (0..80).map(move |column| (column, row)))
        .find(|&(column, row)| resolve(column, row) == visit)
        .expect("the garden draws a clickable usagi for its session");
    assert_eq!(resolve(0, 23), Some(GardenClick::Dismiss));

    let _ = runtime.apply_event(AppEvent::GardenClick(
        resolve(plot.0, plot.1).expect("the click landed on the garden"),
    ));
    assert_eq!(runtime.state().active(), Some(session));
    assert!(matches!(
        runtime.state().route(),
        Route::Home(HomeMode::Closeup)
    ));
    // Home is back: the frame after the visit is no longer the garden.
    let after = render_home_material(&material(&runtime)).join("\n");
    assert!(!after.contains("any key to return"));
}

#[test]
fn garden_routes_click_and_pointer_down_through_the_drawn_frame_hit_test() {
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
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut pointer_gesture = false;

    for key in [
        Key::Click { column: 0, row: 23 },
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 0,
            row: 23,
        }),
    ] {
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        let material = home_frame_material(
            24,
            80,
            &runtime,
            "demo",
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            now(),
        );
        assert_eq!(
            route_garden_input(
                &mut ui,
                &mut runtime,
                Some(&material),
                &key,
                &mut pointer_gesture,
            ),
            Some(GardenInputRoute::Local(Vec::new())),
        );
        assert_eq!(runtime.state().overlay(), None);
        if pointer_gesture {
            assert_eq!(
                route_garden_input(
                    &mut ui,
                    &mut runtime,
                    None,
                    &Key::Pointer(PointerEvent {
                        kind: PointerKind::Up,
                        column: 0,
                        row: 23,
                    }),
                    &mut pointer_gesture,
                ),
                Some(GardenInputRoute::Local(Vec::new())),
            );
        }
    }
    assert!(!pointer_gesture);
}

#[test]
fn garden_frame_material_uses_every_open_projects_projection() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone()]).unwrap();
    let session = alpha.session_ids[0];
    let sessions = vec![ProjectedSession::from_record(
        session,
        &alpha.state.sessions[0],
    )];
    let mut runtime = WorkspaceRuntime::new(alpha.workspace_id, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));

    // One material build per frame, exactly as the loop does it: the deck
    // projection replaces the active-only Garden rather than folding twice.
    let build = |deck: &WorkspaceDeck| {
        home_frame_material(
            24,
            220,
            &runtime,
            "alpha",
            &sessions,
            None,
            health(),
            &BTreeMap::new(),
            None,
            None,
            now(),
        )
        .with_workspace_deck_garden(deck)
    };
    let material = build(&deck);
    let plots = material
        .projection
        .garden_sessions()
        .expect("the idle Home frame is the Garden");

    assert_eq!(plots.len(), 2);
    assert_eq!(plots[0].label, "alpha / alpha-session");
    assert_eq!(plots[1].label, "beta / beta-session");
    // Until the observation lane reaches it, the other project is drawn
    // read-only: no rabbit is invented for a membership nobody observed.
    assert!(!plots[1].agents_observed);
    let read_only = render_home_material(&material);
    assert!(read_only.iter().any(|row| row.contains("project inactive")));

    // The daemon answers for whichever workspace the request names, so the
    // other project's Agents reach its plot without a resident controller.
    let runtime_id = AgentRuntimeId::new();
    assert!(deck.apply_garden_inventory(&AgentWorkspaceObservation {
        inventory: AgentInventory {
            workspace_id: beta.workspace_id,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef {
                    agent_runtime_id: runtime_id,
                    terminal: TerminalRef {
                        daemon_generation: DaemonGeneration::new(),
                        terminal_id: TerminalId::new(),
                        workspace_id: beta.workspace_id,
                        session_id: Some(beta.session_ids[0]),
                        worktree_id: WorktreeId::new(),
                    },
                    session_id: Some(beta.session_ids[0]),
                },
                continuation: AgentContinuationRef::new(),
                state: AgentRuntimeInventoryState::Live,
                resumed_from: None,
            }],
            resumable: Vec::new(),
        },
        session_statuses: BTreeMap::from([(
            beta.session_ids[0],
            usagi_core::domain::agent::AgentStatus::Idle,
        )]),
    }));
    let material = build(&deck);
    let plots = material
        .projection
        .garden_sessions()
        .expect("the idle Home frame is the Garden");
    assert_eq!(
        plots[1]
            .agents
            .iter()
            .map(|agent| (agent.runtime_id, agent.phase))
            .collect::<Vec<_>>(),
        vec![(runtime_id, AgentPhase::Running)]
    );
    let observed = render_home_material(&material);
    assert!(!observed.iter().any(|row| row.contains("project inactive")));
    assert!(observed.iter().any(|row| row.contains("2 sessions")));
    assert!(observed.iter().any(|row| row.contains("1 usagi")));
    assert!(observed.iter().any(|row| row.contains("-.-")));
    assert!(observed.iter().any(|row| row.contains("completed")));
    assert!(!observed.iter().any(|row| row.contains("1 run")));
}

#[test]
fn garden_arrow_wakes_home_without_reaching_the_surface_behind_it() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let mut material = home_frame_material(
        23,
        80,
        &runtime,
        "demo",
        &[],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    material.projection = material.projection.with_deck_garden(
            "5 open projects".to_owned(),
            (0..5)
                .map(|index| {
                    (
                        WorkspaceId::new(),
                        crate::presentation::widgets::garden::GardenSession {
                            sidebar: crate::presentation::widgets::garden::sidebar::SessionDetails::default(),
                            id: SessionId::new(),
                            label: format!("project-{index} / session-{index}"),
                            lifecycle: SessionLifecycle::Available,
                            failure_summary: None,
                            agents_observed: false,
                            agents: Vec::new(),
                            agent_status: None,
                            pending_decisions: 0,
                            pr_merged: false,
                        },
                    )
                })
                .collect(),
        );
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut pointer_gesture = false;

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            Some(&material),
            &Key::Right,
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert_eq!(runtime.state().overlay(), None);
}

#[test]
fn documented_garden_minimum_includes_the_project_bar_row() {
    assert!(garden_fits(14 - PROJECT_BAR_ROWS, 64));
    assert!(!garden_fits(13 - PROJECT_BAR_ROWS, 64));
}

#[test]
fn garden_list_keys_and_wheel_scroll_without_waking_the_terminal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut gesture = false;
    let mut material = home_frame_material(
        24,
        120,
        &runtime,
        "demo",
        &[],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    material.projection = material.projection.with_deck_garden(
        "demo".into(),
        vec![(
            workspace,
            crate::presentation::widgets::garden::GardenSession {
                sidebar: crate::presentation::widgets::garden::sidebar::SessionDetails::default(),
                id: session,
                label: "many agents".into(),
                lifecycle: SessionLifecycle::Available,
                failure_summary: None,
                agents_observed: true,
                agents: (0..30)
                    .map(|_| crate::presentation::widgets::garden::GardenAgent {
                        runtime_id: AgentRuntimeId::new(),
                        phase: AgentPhase::Running,
                    })
                    .collect(),
                agent_status: None,
                pending_decisions: 0,
                pr_merged: false,
            },
        )],
    );
    for (key, expected) in [
        (Key::Down, 1),
        (Key::Up, 0),
        (Key::PageDown, 13),
        (Key::PageUp, 0),
        (
            Key::Live(LiveTerminalAction::Wheel {
                up: false,
                column: 119,
                row: 5,
                notches: 3,
            }),
            3,
        ),
        (
            Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: 119,
                row: 5,
                notches: 3,
            }),
            0,
        ),
    ] {
        assert_eq!(
            route_garden_input(&mut ui, &mut runtime, Some(&material), &key, &mut gesture),
            Some(GardenInputRoute::Local(Vec::new()))
        );
        assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));
        assert_eq!(runtime.state().garden_sidebar_scroll(), expected);
    }
    let wheel_outside = Key::Live(LiveTerminalAction::Wheel {
        up: false,
        column: 0,
        row: 5,
        notches: 1,
    });
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            Some(&material),
            &wheel_outside,
            &mut gesture
        ),
        Some(GardenInputRoute::Local(Vec::new()))
    );
    assert_eq!(runtime.state().overlay(), None);
}

#[test]
fn garden_list_scroll_survives_the_shell_gate_and_redraws_before_the_next_input() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Management {
            action: AppKey::OpenGarden,
            passthrough: Vec::new(),
        },
        Key::Down,
        Key::Down,
        Key::Up,
        Key::PageDown,
        Key::PageUp,
        Key::Live(LiveTerminalAction::Wheel {
            up: false,
            column: 119,
            row: 5,
            notches: 3,
        }),
        Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 119,
            row: 5,
            notches: 3,
        }),
        Key::Click {
            column: 119,
            row: 24,
        },
        Key::Click {
            column: 85,
            row: 24,
        },
        Key::Quit,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    term.size = Some((25, 120));
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
    let snapshot = snapshot_with_sessions(
        "scroll-list",
        &[
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        ],
    );
    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
        Exit::Quit
    );
    let gardens = term
        .frames
        .iter()
        .filter(|frame| frame.join("\n").contains("Garden Action Center"))
        .collect::<Vec<_>>();
    assert_eq!(gardens.len(), 10, "every list input redraws the Garden");
    let top = |frame: &Vec<String>| {
        crate::presentation::widgets::strip_ansi(&frame[3])
            .rsplit('│')
            .next()
            .unwrap()
            .trim()
            .to_owned()
    };
    assert!(top(gardens[0]).contains("scroll-list"));
    assert!(top(gardens[1]).contains("alpha"));
    assert!(top(gardens[2]).contains("usagi/alpha"));
    assert_eq!(top(gardens[3]), top(gardens[1]));
    assert_ne!(top(gardens[4]), top(gardens[3]));
    assert_eq!(top(gardens[5]), top(gardens[0]));
    assert!(top(gardens[6]).contains("No agent activity."));
    assert_eq!(top(gardens[7]), top(gardens[0]));
    assert_eq!(top(gardens[8]), top(gardens[4]));
    assert_eq!(top(gardens[9]), top(gardens[0]));
}

#[test]
fn narrow_garden_list_input_wakes_home_through_the_frame_loop() {
    for key in [
        Key::Down,
        Key::Live(LiveTerminalAction::Wheel {
            up: false,
            column: 79,
            row: 5,
            notches: 3,
        }),
    ] {
        let mut term = FakeTerminal::with_keys(&[
            Key::Management {
                action: AppKey::OpenGarden,
                passthrough: Vec::new(),
            },
            key,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        term.size = Some((25, 80));
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
            run_workspace_controller_with_backend(&mut term, snapshot("narrow"), &mut factory)
                .unwrap(),
            Exit::Quit
        );
        assert_eq!(
            term.frames
                .iter()
                .filter(|frame| { frame.join("\n").contains("Garden Action Center") })
                .count(),
            1,
            "the list input wakes a Garden without a sidebar"
        );
    }
}

#[test]
fn garden_routes_an_inactive_projects_agent_row_to_the_deck_shell() {
    let workspace = WorkspaceId::new();
    let foreign_workspace = WorkspaceId::new();
    let session = SessionId::new();
    let agent = AgentRuntimeId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let mut material = home_frame_material(
        24,
        120,
        &runtime,
        "demo",
        &[],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    material.projection = material.projection.with_deck_garden(
        "2 open projects".to_owned(),
        vec![(
            foreign_workspace,
            crate::presentation::widgets::garden::GardenSession {
                sidebar: crate::presentation::widgets::garden::sidebar::SessionDetails::default(),
                id: session,
                label: "other / review".to_owned(),
                lifecycle: SessionLifecycle::Available,
                failure_summary: None,
                agents_observed: true,
                agents: vec![crate::presentation::widgets::garden::GardenAgent {
                    runtime_id: agent,
                    phase: AgentPhase::Waiting,
                }],
                agent_status: Some(usagi_core::domain::agent::AgentStatus::Running),
                pending_decisions: 0,
                pr_merged: false,
            },
        )],
    );
    let click = (0..24)
        .flat_map(|row| (85..120).map(move |column| (column, row)))
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
                    workspace,
                    agent: Some(runtime),
                    ..
                }) if workspace == foreign_workspace && runtime == agent
            )
        })
        .expect("the inactive project's Agent row is clickable");
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut pointer_gesture = false;

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
        Some(GardenInputRoute::Project(
            crate::presentation::GardenProjectVisit {
                workspace: foreign_workspace,
                session,
                agent: Some(agent),
            }
        )),
    );
    assert_eq!(runtime.state().overlay(), None);
}

/// Any other press is the documented wake-up: it is consumed, and the Home
/// from before the screen saver comes back with no target changed.
#[test]
fn a_click_beside_the_usagi_only_wakes_the_garden() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));

    let before = runtime.state().active();
    let _ = runtime.apply_event(AppEvent::GardenClick(GardenClick::Dismiss));
    assert_eq!(runtime.state().overlay(), None);
    assert_eq!(runtime.state().active(), before);
}

#[test]
fn garden_consumes_every_kind_of_user_input_before_the_terminal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pointer_gesture = false;
    for key in user_interactions() {
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));
        assert_eq!(
            route_garden_input(&mut ui, &mut runtime, None, &key, &mut pointer_gesture,),
            Some(GardenInputRoute::Local(Vec::new())),
            "Garden did not consume {key:?}",
        );
        assert_eq!(runtime.state().overlay(), None);
        if pointer_gesture {
            assert_eq!(
                route_garden_input(
                    &mut ui,
                    &mut runtime,
                    None,
                    &Key::Pointer(PointerEvent {
                        kind: PointerKind::Up,
                        column: 0,
                        row: 0,
                    }),
                    &mut pointer_gesture,
                ),
                Some(GardenInputRoute::Local(Vec::new())),
            );
        }
        assert!(!pointer_gesture);
    }

    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &Key::Other,
            &mut pointer_gesture,
        ),
        None,
    );
    assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));
}

#[test]
fn garden_pointer_press_owns_its_drag_and_release_after_dismissal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let mut pointer_gesture = false;
    let pointer = |kind| {
        Key::Pointer(PointerEvent {
            kind,
            column: 4,
            row: 9,
        })
    };

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Drag),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Up),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(!pointer_gesture);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Down),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert_eq!(runtime.state().overlay(), None);
    assert!(pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Drag),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Up),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(!pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Up),
            &mut pointer_gesture,
        ),
        None,
    );
}

#[test]
fn garden_motion_uses_the_logical_clock_inside_one_relative_time_minute() {
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
    let runtime_id =
        AgentRuntimeId::parse("00000000-0000-4000-8000-000000000001").expect("fixture runtime id");
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
    };
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::RuntimePhase {
        runtime: AgentRuntimeRef {
            agent_runtime_id: runtime_id,
            terminal,
            session_id: Some(session),
        },
        phase: usagi_core::domain::session_lifecycle::AgentPhase::Running,
    }));
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let material = |runtime: &WorkspaceRuntime, wall_clock| {
        home_frame_material(
            24,
            100,
            runtime,
            "demo",
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            wall_clock,
        )
    };

    let first = material(&runtime, clock);
    assert!(first.projection.garden_sessions().is_some());
    for _ in 0..7 {
        let _ = runtime.apply_event(AppEvent::Tick);
    }
    assert_eq!(
        material(&runtime, clock + Duration::milliseconds(900)),
        first,
        "the Garden rebuilt at the 16 ms input-pump cadence"
    );

    let _ = runtime.apply_event(AppEvent::Tick);
    let advanced = material(&runtime, clock + Duration::seconds(1));
    assert_eq!(
        first.now, advanced.now,
        "relative-time clock left its minute"
    );
    assert_ne!(
        first.projection.garden_animation_tick(),
        advanced.projection.garden_animation_tick(),
        "the canonical Garden pose did not advance"
    );
    assert_ne!(
        render_home_material(&first),
        render_home_material(&advanced),
        "the running pose froze inside one wall-clock minute"
    );
}

/// The Garden's cross-project lane observes only while the screen saver is
/// on screen, keeps one round in flight at a time, and backs off when the
/// daemon answers nothing. Closing the Garden re-arms it, so the next
/// opening shows the other projects' Agents without waiting out a cadence.
#[test]
fn garden_observation_runs_only_while_the_garden_is_open_and_backs_off_unanswered() {
    let mut lane = crate::presentation::ObservationLane::new(
        crate::presentation::GARDEN_OBSERVATION_INTERVAL,
        crate::presentation::GARDEN_OBSERVATION_BACKOFF,
    );
    let mut now = std::time::Duration::ZERO;
    for _ in 0..1_000 {
        assert!(
            !lane.begin_if_due(false, now),
            "a closed Garden observes nothing"
        );
        now += std::time::Duration::from_millis(16);
    }

    assert!(lane.begin_if_due(true, now), "opening observes immediately");
    assert!(
        !lane.begin_if_due(true, now),
        "one round holds the only port"
    );
    lane.complete(now, true);
    assert!(!lane.begin_if_due(true, now));
    let just_before = (now + crate::presentation::GARDEN_OBSERVATION_INTERVAL)
        .checked_sub(std::time::Duration::from_millis(1))
        .expect("the cadence is longer than a millisecond");
    assert!(!lane.begin_if_due(true, just_before));
    assert!(lane.begin_if_due(true, now + crate::presentation::GARDEN_OBSERVATION_INTERVAL));

    // Nothing answered: the same open Garden waits out the longer backoff.
    lane.complete(now, false);
    assert!(!lane.begin_if_due(
        true,
        now + crate::presentation::GARDEN_OBSERVATION_INTERVAL * 4
    ));
    assert!(lane.begin_if_due(true, now + crate::presentation::GARDEN_OBSERVATION_BACKOFF));

    // A round dispatched before the Garden closed still owns the port, so
    // closing does not admit a second one; once it lands, re-opening
    // observes at once.
    assert!(!lane.begin_if_due(false, now));
    lane.complete(now, true);
    assert!(!lane.begin_if_due(false, now));
    assert!(lane.begin_if_due(true, now));
}

/// One round asks each *other* open project for its own inventory. An
/// answer that names a different workspace is dropped rather than drawn in
/// the plot that was asked about, and the port comes back either way.
#[test]
fn a_garden_round_observes_every_other_project_and_drops_a_mismatched_answer() {
    struct FakeGardenInventory {
        answers: BTreeMap<WorkspaceId, Result<AgentWorkspaceObservation, String>>,
        asked: Arc<Mutex<Vec<WorkspaceId>>>,
    }

    impl crate::presentation::GardenInventoryPort for FakeGardenInventory {
        fn inventory(
            &mut self,
            workspace: WorkspaceId,
        ) -> Result<AgentWorkspaceObservation, String> {
            self.asked.lock().unwrap().push(workspace);
            self.answers
                .remove(&workspace)
                .unwrap_or_else(|| Err("daemon unavailable".to_owned()))
        }
    }

    // An embedder that supplies no Garden lane keeps every other project's
    // plot read-only instead of failing the frame.
    assert!(
        UnavailableGardenInventoryPort
            .inventory(WorkspaceId::new())
            .is_err()
    );

    let observed = WorkspaceId::new();
    let mismatched = WorkspaceId::new();
    let unavailable = WorkspaceId::new();
    let empty_inventory = |workspace| AgentWorkspaceObservation {
        inventory: AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
        session_statuses: BTreeMap::new(),
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let port = FakeGardenInventory {
        answers: [
            (observed, Ok(empty_inventory(observed))),
            // The daemon answered about someone else: that is not evidence
            // about the project this round asked for.
            (mismatched, Ok(empty_inventory(WorkspaceId::new()))),
        ]
        .into_iter()
        .collect(),
        asked: Arc::clone(&asked),
    };
    let (sender, completions) = std::sync::mpsc::channel();
    // More projects than one round observes, so the bound is exercised too.
    let mut targets = vec![observed, mismatched, unavailable];
    targets.extend((0..crate::presentation::MAX_OBSERVED_PROJECTS).map(|_| WorkspaceId::new()));

    crate::presentation::spawn_garden_observation_job(Box::new(port), targets, sender);
    let completion = completions
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the round returns its port");

    assert_eq!(
        completion
            .inventories
            .iter()
            .map(|inventory| inventory.inventory.workspace_id)
            .collect::<Vec<_>>(),
        vec![observed]
    );
    assert_eq!(
        asked.lock().unwrap().len(),
        crate::presentation::MAX_OBSERVED_PROJECTS
    );
}

#[test]
fn deck_preparation_retries_an_empty_lifecycle_snapshot_for_garden() {
    let mut fake = FakeLoader {
        open_snapshot: FakeOpenSnapshot::Empty,
        ..FakeLoader::default()
    };
    let paths = vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")];
    let mut term = FakeTerminal::default();
    let (_, active, mut deck) = prepare_workspace_deck(&mut term, &mut fake, &paths).unwrap();

    assert_eq!(fake.opened, paths);
    assert_eq!(fake.refreshed, paths);
    assert_eq!(active.session_ids.len(), 1);
    assert_eq!(deck.garden_projection(&[]).1.len(), 1);

    fake.opened.clear();
    fake.refreshed.clear();
    let mut loader: Option<&mut dyn WorkspaceLoader> = Some(&mut fake);
    let gamma = prepare_deck_workspace(
        &mut term,
        &mut loader,
        &mut deck,
        Path::new("/tmp/gamma"),
        "Opening…",
    )
    .expect("an added project is prepared");
    assert_eq!(gamma.session_ids.len(), 1);
    assert_eq!(fake.opened, vec![PathBuf::from("/tmp/gamma")]);
    assert_eq!(fake.refreshed, vec![PathBuf::from("/tmp/gamma")]);
}

#[test]
fn garden_hover_never_wakes_activates_or_leaks_to_the_covered_terminal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let key = Key::Pointer(PointerEvent {
        kind: PointerKind::Move,
        column: 10,
        row: 8,
    });
    let mut gesture = false;
    assert!(!garden_shell_owned_wake(&key));
    assert_eq!(
        route_garden_input(&mut ui, &mut runtime, None, &key, &mut gesture),
        Some(GardenInputRoute::Local(Vec::new()))
    );
    assert_eq!(runtime.state().garden_pointer(), None);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenGarden));
    let active = runtime.state().active();
    assert_eq!(
        route_garden_input(&mut ui, &mut runtime, None, &key, &mut gesture),
        Some(GardenInputRoute::Local(Vec::new()))
    );
    assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));
    assert_eq!(runtime.state().garden_pointer(), Some((10, 8)));
    assert_eq!(runtime.state().active(), active);
    assert!(!gesture);
    let _ = runtime.apply_event(AppEvent::GardenClick(GardenClick::Dismiss));
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenGarden));
    assert_eq!(runtime.state().garden_pointer(), None);
}

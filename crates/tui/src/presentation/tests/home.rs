//! Home 画面そのものの presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn app_event_from_key_maps_ordinary_management_keys() {
    assert_eq!(app_event_from_key(Key::Up), Some(AppEvent::Key(AppKey::Up)));
    assert_eq!(
        app_event_from_key(Key::Down),
        Some(AppEvent::Key(AppKey::Down))
    );
    assert_eq!(
        app_event_from_key(Key::PageUp),
        Some(AppEvent::Key(AppKey::PageUp))
    );
    assert_eq!(
        app_event_from_key(Key::PageDown),
        Some(AppEvent::Key(AppKey::PageDown))
    );
    assert_eq!(
        app_event_from_key(Key::Enter),
        Some(AppEvent::Key(AppKey::Enter))
    );
    assert_eq!(
        app_event_from_key(Key::Backspace),
        Some(AppEvent::Key(AppKey::Backspace))
    );
    assert_eq!(
        app_event_from_key(Key::Paste("貼り付け".to_owned())),
        Some(AppEvent::Key(AppKey::Paste("貼り付け".to_owned())))
    );
    assert_eq!(
        app_event_from_key(Key::Tab),
        Some(AppEvent::Key(AppKey::Tab))
    );
    assert_eq!(
        app_event_from_key(Key::Escape),
        Some(AppEvent::Key(AppKey::Escape))
    );
    assert_eq!(
        app_event_from_key(Key::Char('x')),
        Some(AppEvent::Key(AppKey::Char('x')))
    );
    assert_eq!(
        app_event_from_key(Key::Char('\u{1}')),
        Some(AppEvent::Key(AppKey::CtrlA))
    );
    assert_eq!(
        app_event_from_key(Key::Quit),
        Some(AppEvent::Key(AppKey::CtrlC))
    );
    assert_eq!(
        app_event_from_key(Key::CtrlQ),
        Some(AppEvent::Key(AppKey::CtrlQ))
    );
    assert_eq!(
        app_event_from_key(Key::CtrlX),
        Some(AppEvent::Key(AppKey::CtrlX))
    );
    assert_eq!(
        app_event_from_key(Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![0x13],
        }),
        Some(AppEvent::Key(AppKey::SaveRoles))
    );
}

/// A resize is a redraw, never an inventory refresh. It reaches the reducer
/// as the same mascot tick as a wake-up, while the real dimensions come from
/// `term.size()` at the head of the frame; the daemon lanes are not involved
/// at all (#551).
#[test]
fn a_resize_maps_to_a_redraw_tick_and_stays_distinct_from_a_wakeup() {
    assert_eq!(app_event_from_key(Key::Resize), Some(AppEvent::Tick));
    assert_eq!(app_event_from_key(Key::Other), Some(AppEvent::Tick));
    assert_ne!(Key::Resize, Key::Other);
}

#[test]
fn sidebar_pointer_adapter_preserves_coordinates_and_injected_time() {
    let at = std::time::Duration::from_millis(1_234);
    assert_eq!(
        sidebar_pointer_event(3, 4, at),
        AppEvent::Pointer {
            column: 3,
            row: 4,
            at,
        }
    );
}

#[test]
fn pr_modal_click_route_claims_the_box_and_closes_on_its_background() {
    assert_eq!(
        route_pr_modal_click(Some(Overlay::Prs), 24, 80, 0, 2),
        Some(PrModalClickRoute::Inside)
    );
    assert_eq!(
        route_pr_modal_click(Some(Overlay::Prs), 24, 80, 0, 1),
        Some(PrModalClickRoute::Outside)
    );
    assert_eq!(route_pr_modal_click(None, 24, 80, 0, 1), None);
    assert_eq!(
        route_pr_modal_click(Some(Overlay::Notes), 24, 80, 0, 1),
        None
    );
}

/// The screen saver's deadline must survive an Agent working all night and
/// end the moment a person touches the terminal, so the classification of
/// "was that a user?" is pinned over the whole key vocabulary.
#[test]
fn only_a_real_interaction_postpones_the_screen_saver() {
    for key in user_interactions() {
        assert!(is_user_activity(&key), "{key:?} should postpone the garden");
    }
    // The one wake-up that is not a person: frame ticks, drained daemon
    // events, and Agent output all arrive as `Other`.
    assert!(!is_user_activity(&Key::Other));
}

/// The watch is a pure elapsed-time fold: the shell reduces its monotonic
/// clock to a `Duration`, so nothing below it reads a clock at all.
#[test]
fn the_idle_watch_measures_from_the_last_interaction() {
    let ms = std::time::Duration::from_millis;
    let mut watch = IdleWatch::new(ms(1_000));

    assert_eq!(watch.observe(&Key::Other, ms(1_500)), ms(500));
    assert_eq!(watch.observe(&Key::Other, ms(9_000)), ms(8_000));
    // An interaction restarts the measurement in the same call.
    assert_eq!(watch.observe(&Key::Char('a'), ms(9_000)), ms(0));
    assert_eq!(watch.observe(&Key::Other, ms(9_400)), ms(400));
    // A clock that appears to run backwards (it cannot, but the arithmetic
    // must not panic if it ever did) reads as "no idle time".
    assert_eq!(watch.observe(&Key::Other, ms(0)), ms(0));
}

#[test]
#[allow(clippy::too_many_lines)] // One host fixture verifies the complete routing matrix.
fn backend_host_and_explicit_error_adapters_cover_the_full_route_matrix() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    )
    .with_decisions(Box::new(UnavailableBackendPort))
    .with_overlay(Box::new(UnavailableBackendPort));

    for effect in [
        Effect::CreateSession {
            workspace,
            token: PendingToken::from_raw(1),
            operation_id: OperationId::new(),
            intent: SessionCreateIntent {
                name: "feature".to_owned(),
                base_ref: None,
                profile: None,
                model: None,
                role_id: None,
            },
        },
        Effect::RefreshSessions { workspace },
        Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: false,
            purge_orphan: false,
        },
        Effect::SleepSession { workspace, session },
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
            workspace,
            continuation: AgentContinuationRef::new(),
        },
        Effect::OpenTerminal {
            target,
            operation_id: OperationId::new(),
            arguments: "new".to_owned(),
        },
        Effect::OpenExternalTerminal { target },
        Effect::SelectTab {
            direction: TabDirection::Next,
        },
    ] {
        backend.dispatch(effect);
    }
    assert_eq!(actions.try_iter().count(), 10);

    for effect in [
        Effect::LoadNotes { target },
        Effect::SaveNotes {
            target,
            scratchpad: Scratchpad::default(),
        },
        Effect::LoadEnvironment {
            scope: EnvScope::Workspace,
        },
        Effect::SaveEnvironment {
            scope: EnvScope::Workspace,
            entries: vec![EnvironmentEntry {
                name: "KEY".to_owned(),
                value: "value".to_owned(),
            }],
        },
        Effect::WorkspaceCommand {
            workspace,
            command: crate::usecase::overview::Command::Issue {
                arguments: "list".to_owned(),
            },
        },
        Effect::RefreshDecisions { workspace },
        Effect::ResolveDecision {
            workspace,
            decision_id: UserDecisionId::new(),
            answer: UserDecisionAnswer::Freeform {
                text: "answer".to_owned(),
            },
        },
        Effect::LoadPullRequests { target },
        Effect::LoadPreview {
            target,
            request_id: RequestId::new(),
            path: None,
            filter: PreviewFileFilter::All,
        },
        Effect::OpenPullRequest {
            url: "https://github.com/o/r/pull/1".to_owned(),
        },
    ] {
        backend.dispatch(effect);
    }
    assert_eq!(backend.drain_events().len(), 10);
}

#[test]
fn default_agent_port_rejects_legacy_inventory_and_exact_resume() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut port = SuccessfulAgentPort(live_terminal_ref(workspace, session));
    assert_eq!(
        port.resume(workspace, session, OperationId::new())
            .unwrap_err(),
        "Agent resume is unavailable."
    );
    assert_eq!(
        port.resume_inventory(workspace).unwrap_err(),
        "Agent resume inventory is unavailable."
    );
    let target = usagi_core::domain::agent::AgentResumeTarget {
        continuation: usagi_core::domain::id::AgentContinuationRef::new(),
        source: usagi_core::domain::id::AgentResumeSourceId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
        runtime_id: usagi_core::domain::id::AgentRuntimeId::new(),
        adapter_revision: 1,
    };
    assert_eq!(
        port.resume_exact(target, OperationId::new()).unwrap_err(),
        "Exact Agent resume is unavailable."
    );
}

#[test]
fn a_failed_lifecycle_flows_to_the_sidebar_rows_and_the_reducer() {
    use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    view.set_session_lifecycles(std::collections::BTreeMap::from([(
        session,
        SessionLifecycleProjection {
            lifecycle: SessionLifecycle::Failed,
            failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Create),
            failure_summary: Some("create failed".into()),
        },
    )]));
    let role = crate::usecase::application::controller::SessionRoleProjection {
        role_id: None,
        role_summary: Some("Reviewer".into()),
        parent_session_id: None,
        agent_status: None,
    };
    view.set_session_roles(BTreeMap::from([(session, role.clone())]));
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));

    // The projected sidebar row carries the Failed lifecycle and its reason.
    let mut state =
        crate::usecase::application::controller::AppState::home(workspace, vec![session]);
    let _ = crate::usecase::application::controller::update(
        &mut state,
        crate::usecase::application::controller::AppEvent::Backend(
            crate::usecase::application::controller::BackendEvent::PullRequestsLoaded {
                target: Target::Session(session),
                revision: 1,
                prs: vec![usagi_core::domain::pr_inventory::PrEntry::new(
                    usagi_core::domain::pr_inventory::canonicalize(
                        "https://github.com/example/repository/pull/1545",
                    )
                    .unwrap(),
                )],
            },
        ),
    );
    let rows = crate::presentation::project_controller_sessions(&ui, &state);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(rows[0].failure_summary.as_deref(), Some("create failed"));
    assert!(!rows[0].removing);
    assert!(rows[0].pr_count > 0);

    // The reducer receives the lifecycle so it can gate attach by capability.
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    crate::presentation::sync_runtime_sessions(&mut runtime, &ui, &[]);
    assert_eq!(runtime.state().sessions(), &[session]);
    assert_eq!(runtime.state().session_roles().get(&session), Some(&role));
    assert_eq!(
        runtime
            .state()
            .session_lifecycles()
            .get(&session)
            .map(|projection| projection.lifecycle),
        Some(SessionLifecycle::Failed),
    );
}

#[test]
fn sidebar_groups_children_and_navigation_survives_snapshot_refresh() {
    use crate::usecase::application::controller::{
        AppEvent, AppKey, Selection, SessionRoleProjection, Target,
    };

    let ids = (0..6).map(|_| SessionId::new()).collect::<Vec<_>>();
    let mut snapshot = state("demo");
    let template = snapshot.sessions[0].clone();
    snapshot.sessions = [
        "session",
        "agy",
        "review",
        "inject-user-environment",
        "display",
        "grandchild",
    ]
    .into_iter()
    .map(|name| SessionRecord {
        name: name.into(),
        ..template.clone()
    })
    .collect();
    let records = snapshot.sessions.clone();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), snapshot, ids.clone());
    let roles = BTreeMap::from([(ids[3], ids[0]), (ids[5], ids[3])].map(|(id, parent)| {
        (
            id,
            SessionRoleProjection {
                role_id: None,
                role_summary: None,
                parent_session_id: Some(parent),
                agent_status: None,
            },
        )
    }));
    view.set_session_roles(roles.clone());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(WorkspaceId::new(), vec![ids[0]]);
    crate::presentation::sync_runtime_sessions(&mut runtime, &ui, &[]);
    let expected = [ids[0], ids[3], ids[5], ids[1], ids[2], ids[4]];
    assert_eq!(runtime.state().sessions(), &expected);
    let rows = crate::presentation::project_controller_sessions(&ui, runtime.state());
    assert_eq!(
        rows.iter()
            .map(|row| (row.label.as_str(), row.organization_depth))
            .collect::<Vec<_>>(),
        vec![
            ("session", 0),
            ("inject-user-environment", 1),
            ("grandchild", 2),
            ("agy", 0),
            ("review", 0),
            ("display", 0),
        ]
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Down));
    assert_eq!(
        runtime.state().selected(),
        Selection::Target(Target::Session(ids[3]))
    );
    let active = runtime.state().active();
    ui.workspace.replace_sessions_with_runtime_ids(records, ids);
    ui.workspace.set_session_roles(roles);
    crate::presentation::sync_runtime_sessions(&mut runtime, &ui, &[]);
    assert_eq!(runtime.state().sessions(), &expected);
    assert_eq!(
        runtime.state().selected(),
        Selection::Target(Target::Session(expected[1]))
    );
    assert_eq!(runtime.state().active(), active);
    let revision = ui.workspace.material_revision();
    ui.workspace
        .set_session_roles(ui.workspace.session_roles().clone());
    assert_eq!(ui.workspace.material_revision(), revision);
}

#[test]
fn a_deleting_lifecycle_keeps_the_row_marked_removing_without_a_local_command() {
    use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
    let session = SessionId::new();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    view.set_session_lifecycles(std::collections::BTreeMap::from([(
        session,
        SessionLifecycleProjection {
            lifecycle: SessionLifecycle::Deleting,
            failure_stage: None,
            failure_summary: None,
        },
    )]));
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));

    // The daemon accepts a removal before its worktree teardown runs, so the
    // row stays marked as being removed on the strength of the daemon's
    // lifecycle alone — this TUI never issued the command.
    let state =
        crate::usecase::application::controller::AppState::home(WorkspaceId::new(), vec![session]);
    let rows = crate::presentation::project_controller_sessions(&ui, &state);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].lifecycle, SessionLifecycle::Deleting);
    assert!(rows[0].removing);
}

#[test]
fn launch_admission_is_bounded_and_refuses_beyond_the_queue_with_one_busy_completion() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let launched = scoped_terminal_ref(workspace, Some(session));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, _finished) = std::sync::mpsc::channel();
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
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let mut admitted = Vec::new();
    for _ in 0..crate::presentation::PANE_LAUNCH_QUEUE_LIMIT {
        let operation = OperationId::new();
        admitted.push(operation);
        crate::presentation::enqueue_pane_launch(
            &mut ui,
            agent_launch(workspace, session, operation),
        );
    }
    assert_eq!(
        ui.pane_launches.len(),
        crate::presentation::PANE_LAUNCH_QUEUE_LIMIT
    );

    // The queue is full: the next requests never reach the daemon and each
    // completes exactly once as Busy — Agent, generic terminal, and explicit
    // tab resume alike.
    let history = interrupted_history(workspace, Some(session), true);
    let refused_kinds = [
        agent_launch(workspace, session, OperationId::new()),
        crate::presentation::PaneLaunch::Terminal {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            arguments: "new".into(),
        },
        crate::presentation::PaneLaunch::ResumeExact {
            operation: OperationId::new(),
            continuation: history.continuation,
            target: history.target.unwrap(),
        },
    ];
    let refused = refused_kinds
        .iter()
        .map(|launch| launch.identity().operation())
        .collect::<Vec<_>>();
    for (launch, operation) in refused_kinds.into_iter().zip(refused.iter().copied()) {
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        pending.insert(operation, target);
        crate::presentation::enqueue_pane_launch(&mut ui, launch);
    }
    assert_eq!(
        ui.pane_launches.len(),
        crate::presentation::PANE_LAUNCH_QUEUE_LIMIT
    );

    // One worker is admitted first, so the Busy completions must not free it.
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    let active = ui.active_pane_launch;
    assert!(active.is_some());
    let outcomes = drain_completions(&mut ui, &mut runtime, &mut pending, refused.len());
    for (outcome, operation) in outcomes.iter().zip(refused.iter().copied()) {
        let (completed, message) = match outcome {
            crate::presentation::PaneLaunchOutcome::Agent { operation, result } => {
                (*operation, result.as_ref().err().cloned())
            }
            crate::presentation::PaneLaunchOutcome::Terminal { operation, result } => {
                (*operation, result.as_ref().err().cloned())
            }
            crate::presentation::PaneLaunchOutcome::ResumeExact {
                operation, result, ..
            } => (*operation, result.as_ref().err().cloned()),
        };
        assert_eq!(completed, operation);
        assert_eq!(
            message.as_deref(),
            Some(crate::presentation::PANE_LAUNCH_BUSY)
        );
    }
    // A Busy completion is unadmitted: it never frees the running worker.
    assert_eq!(ui.active_pane_launch, active);
    assert!(pending.is_empty());
    assert!(ui.pane_completions.try_recv().is_err());
    release.send(()).unwrap();
}

#[test]
fn select_tab_host_action_is_inert_without_an_active_target_or_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut pending = std::collections::HashMap::new();

    for mut runtime in [
        WorkspaceRuntime::new(workspace, Vec::new()),
        WorkspaceRuntime::new(workspace, vec![session]),
    ] {
        let session_ids = runtime.state().sessions().to_vec();
        let view_state = if session_ids.is_empty() {
            empty_state("demo")
        } else {
            state("demo")
        };
        let view = WorkspaceView::with_runtime_ids(ws("demo"), view_state, session_ids);
        let mut command_lane = SessionCommandLane::new();
        let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort));
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::SelectTab(TabDirection::Next))
            .unwrap();
        drain_host_actions(
            &receiver,
            &mut ui,
            &mut command_lane,
            &mut runtime,
            &mut pending,
        );
        assert!(runtime.focused_terminal().is_none());
    }
}

#[test]
fn compatibility_ports_fail_explicitly_and_never_silently_succeed() {
    struct DefaultSessionPort;
    impl SessionCommandPort for DefaultSessionPort {}

    let workspace_id = WorkspaceId::new();
    let session_id = SessionId::new();
    let workspace = ws("fallback");
    assert!(
        DefaultSessionPort
            .execute(&workspace, None, SessionCommand::List)
            .is_err()
    );
    assert!(
        UnavailableSessionCommandPort
            .execute(&workspace, None, SessionCommand::List)
            .is_err()
    );
    assert!(
        UnavailableAgentCommandPort
            .launch(OperationId::new(), workspace_id, None, None)
            .is_err()
    );
    assert!(
        UnavailableAgentCommandPort
            .launch_goal(OperationId::new(), workspace_id, None, "goal")
            .is_err()
    );
    // An embedder without a launch client refuses every pane launch inline
    // instead of leaving a pending tab forever.
    let history = interrupted_history(workspace_id, Some(session_id), true);
    assert!(
        UnavailablePaneLaunchPort
            .launch(OperationId::new(), workspace_id, None, None)
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .launch_goal(OperationId::new(), workspace_id, None, "goal")
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .resume(workspace_id, session_id, OperationId::new())
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .resume_exact(history.target.unwrap(), OperationId::new())
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .launch_terminal(
                workspace_id,
                None,
                terminal_geometry(20, 80),
                "new",
                OperationId::new(),
            )
            .is_err()
    );

    let decision_id = UserDecisionId::new();
    assert!(matches!(
        UnavailableDecisionCommandPort.refresh(workspace_id),
        BackendEvent::Notice(_)
    ));
    assert!(matches!(
        UnavailableDecisionCommandPort.resolve(
            workspace_id,
            decision_id,
            UserDecisionAnswer::Option {
                option_id: "safe".to_owned(),
            },
        ),
        BackendEvent::DecisionError { .. }
    ));
    assert!(matches!(
        UnavailableEnvironmentStore.load(EnvScope::Workspace),
        BackendEvent::EnvironmentError { .. }
    ));
    assert!(matches!(
        UnavailableEnvironmentStore.save(EnvScope::Global, Vec::new()),
        BackendEvent::EnvironmentError { .. }
    ));
    assert!(UnavailablePrSnapshotPort.snapshot(session_id).is_err());
    assert!(
        UnavailableBrowserOpener
            .open("https://example.com")
            .is_err()
    );
    NoDesktopNotifications.notify("title", "body");

    let mut settings = DefaultSettingsPort;
    settings
        .save(SettingsScope::Global, &Settings::default())
        .unwrap();
}

#[test]
fn serialized_launch_port_forwards_goal_admission() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let port = launch_port(Box::new(SuccessfulAgentPort(terminal.clone())));
    let admitted = port
        .launch_goal(OperationId::new(), workspace, None, "prepare a PR")
        .unwrap();
    assert!(admitted.terminal.fences(&terminal));
}

#[test]
fn closeup_environment_editor_is_composited_over_home() {
    use crate::presentation::views::workspace::ProjectedSession;
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{AppEvent, Effect};

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let _ = runtime.handle_key(Key::Enter);
    for _ in 0..3 {
        let _ = runtime.handle_key(Key::Down);
    }
    assert!(matches!(
        runtime.handle_key(Key::Enter).as_slice(),
        [Effect::LoadEnvironment {
            scope: EnvScope::Workspace
        }]
    ));
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::EnvironmentLoaded {
        scope: EnvScope::Workspace,
        entries: Vec::new(),
        inherited: Vec::new(),
    }));
    let sessions = [ProjectedSession {
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
    }];
    let frame = render_controller_frame(
        20,
        80,
        &runtime,
        "atlas",
        &sessions,
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
    )
    .join("\n");
    assert!(frame.contains("Environment"));
    assert!(frame.contains("workspace env only (global values stay unchanged)"));
    assert!(frame.contains("one NAME=value binding per line"));
}

#[test]
fn unavailable_backend_reports_role_editor_errors_for_load_and_save() {
    use crate::usecase::application::daemon_backend::TargetStorePort as _;

    let mut port = UnavailableBackendPort;
    let (load, load_events) = Completions::channel();
    port.load_roles(RoleEditorScope::Workspace, load);
    assert!(matches!(
        load_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::RolesError {
            scope: RoleEditorScope::Workspace,
            ..
        })
    ));

    let (save, save_events) = Completions::channel();
    port.save_roles(RoleEditorScope::Global, "version = 1\n".to_owned(), save);
    assert!(matches!(
        save_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::RolesError {
            scope: RoleEditorScope::Global,
            ..
        })
    ));
}

#[test]
fn unavailable_backend_reports_pr_copy_and_dismiss_errors() {
    use crate::usecase::application::daemon_backend::OverlayPort as _;

    let mut port = UnavailableBackendPort;
    let (copy, copy_events) = Completions::channel();
    port.copy_pull_request("https://github.com/o/r/pull/7".to_owned(), copy);
    assert!(matches!(
        copy_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(_))
    ));

    let (dismiss, dismiss_events) = Completions::channel();
    port.dismiss_pull_request(
        SessionId::new(),
        "https://github.com/o/r/pull/7".to_owned(),
        dismiss,
    );
    assert!(matches!(
        dismiss_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(_))
    ));
}

/// #554 acceptance, through the real frame loop: an idle Home reaches
/// neither the filesystem nor the renderer on a tick that changes nothing,
/// while every drain still runs on exactly those ticks.
#[test]
fn idle_ticks_skip_the_worktree_scan_and_the_redraw_but_never_a_drain() {
    reset_projection_build_counts();
    let scans = Arc::new(AtomicUsize::new(0));
    let lane_drains = Arc::new(AtomicUsize::new(0));

    let ticks = 1_000;
    let mut keys = vec![Key::Other; ticks];
    keys.extend([Key::CtrlQ, Key::Char('y')]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: Some(Box::new(FakeSessionRefreshPort {
            wakes: Arc::default(),
            takes: Arc::clone(&lane_drains),
            queued: Arc::default(),
        })),
        decisions: None,
        session_worktrees: Some(counting_scan(&scans)),
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot("idle"), &mut factory).unwrap(),
        Exit::Quit
    );

    assert_eq!(
        scans.load(Ordering::SeqCst),
        0,
        "an idle frame reached the sessions directory"
    );
    let frames = term.frames.len();
    assert!(
        frames < ticks,
        "{frames} draws for {ticks} ticks: the redraw gate did nothing"
    );
    // The floor is the rabbit: it has three distinct appearances per six
    // ticks and #554 keeps that cadence, so roughly half the idle ticks are
    // genuinely material.
    assert!(
        frames <= ticks / 2 + 4,
        "{frames} draws for {ticks} ticks is above the animation floor"
    );
    // Every iteration still drained the resident lane, including the ones
    // that drew nothing.
    assert!(lane_drains.load(Ordering::SeqCst) >= ticks);
    let (session_builds, terminal_builds) = projection_build_counts();
    assert_eq!(
        session_builds, 1,
        "session rows/cwd were rebuilt on idle ticks"
    );
    assert_eq!(
        terminal_builds, 1,
        "terminal viewport/link projection was rebuilt on idle ticks"
    );
}

/// #551 acceptance: several `RefreshSessions` inside one cadence period are
/// answered by the one snapshot the lane publishes, and a lane that never
/// answers parks at most one completion instead of accumulating them.
#[test]
fn refresh_requests_coalesce_onto_one_published_snapshot() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending_targets = std::collections::HashMap::new();
    let mut pending_refresh = None;
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    );
    let wakes = Arc::new(AtomicUsize::new(0));
    let published = SessionId::new();
    let mut lane = FakeSessionRefreshPort {
        wakes: Arc::clone(&wakes),
        takes: Arc::default(),
        queued: Arc::new(Mutex::new(VecDeque::from([Ok(SessionCommandResult {
            message: "daemon snapshot refreshed".to_owned(),
            sessions: Some(ui.workspace.sessions().to_vec()),
            session_ids: Some(vec![published]),
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: Some(7),
        })]))),
    };

    for _ in 0..3 {
        backend.dispatch(Effect::RefreshSessions { workspace });
    }
    crate::presentation::drain_controller_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending_targets,
        &mut lane,
        &mut pending_refresh,
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 3);
    assert!(pending_refresh.is_some());

    crate::presentation::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(pending_refresh.is_none());
    assert_eq!(ui.workspace.session_ids(), &[published]);
    assert_eq!(ui.last_session_revision, 7);
    let events = backend.drain_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        AppEvent::Backend(BackendEvent::Sessions(ids)) if ids == &[published]
    ));

    // A second drain with nothing published leaves the frame untouched.
    crate::presentation::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(backend.drain_events().is_empty());

    // Rows without stable identities are not actionable: the projection and
    // completion both fail closed instead of retaining the previous target.
    let (completions, events) = crate::usecase::application::daemon_backend::Completions::channel();
    pending_refresh = Some(completions);
    lane.queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push_back(Ok(SessionCommandResult {
            message: "legacy snapshot".to_owned(),
            sessions: Some(ui.workspace.sessions().to_vec()),
            session_ids: None,
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: Some(8),
        }));
    crate::presentation::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(ui.workspace.sessions().is_empty());
    assert!(ui.workspace.session_ids().is_empty());
    assert!(matches!(
        events.try_recv().unwrap(),
        AppEvent::Backend(BackendEvent::Sessions(ids)) if ids.is_empty()
    ));
}

/// A lane that fails reports it once through the parked completion and
/// leaves the adopted snapshot alone, and a snapshot older than one already
/// adopted is discarded whichever lane observed it.
#[test]
fn a_failed_or_stale_lane_observation_never_rewrites_the_adopted_snapshot() {
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    ui.last_session_revision = 9;
    let (completions, events) = crate::usecase::application::daemon_backend::Completions::channel();
    let mut pending_refresh = Some(completions);
    let stale = SessionId::new();
    let mut lane = FakeSessionRefreshPort {
        wakes: Arc::default(),
        takes: Arc::default(),
        queued: Arc::new(Mutex::new(VecDeque::from([
            Err("daemon unavailable\ninternal detail".to_owned()),
            Ok(SessionCommandResult {
                message: "stale".to_owned(),
                sessions: Some(Vec::new()),
                session_ids: Some(vec![stale]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(3),
            }),
            Err("later daemon failure".to_owned()),
        ]))),
    };

    crate::presentation::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(pending_refresh.is_none());
    assert!(matches!(
        events.try_recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(notice))
            if notice.message == "daemon unavailable"
    ));
    assert_eq!(ui.workspace.session_ids(), &[session]);

    crate::presentation::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert_eq!(ui.workspace.session_ids(), &[session]);
    assert_eq!(ui.last_session_revision, 9);

    // A lane error without a parked reducer completion is intentionally
    // consumed without synthesizing an event.
    crate::presentation::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(events.try_recv().is_err());
}

#[test]
fn concurrent_create_create_completes_second_as_busy() {
    assert_busy_pair(
        ConcurrentSessionRequest::Create(1),
        ConcurrentSessionRequest::Create(2),
    );
}

#[test]
fn concurrent_create_remove_completes_second_as_busy() {
    assert_busy_pair(
        ConcurrentSessionRequest::Create(1),
        ConcurrentSessionRequest::Remove,
    );
}

#[test]
fn concurrent_remove_create_completes_second_as_busy() {
    assert_busy_pair(
        ConcurrentSessionRequest::Remove,
        ConcurrentSessionRequest::Create(2),
    );
}

#[test]
fn a_background_tab_is_watched_by_scope_inventory_and_never_attached_or_resumed() {
    let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
    let exited = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime, foreground, background) =
        foreground_and_background_panes(Box::new(BackgroundLanePort {
            log: Arc::clone(&log),
            exited: Arc::clone(&exited),
        }));

    close_exited_panes(&mut ui, &mut runtime);

    let recorded = log.lock().unwrap();
    assert_eq!(
        recorded.watched.last().cloned(),
        Some(vec![background.clone()]),
        "only the detached background tab is observed by scope inventory"
    );
    assert!(
        !recorded.polls.iter().any(|polled| polled == &background),
        "a background tab is never resumed"
    );
    assert!(
        !recorded
            .attaches
            .iter()
            .skip(1)
            .any(|attached| attached == &background),
        "a background tab is never re-attached once it leaves the foreground"
    );
    assert_eq!(
        recorded.polls,
        vec![foreground],
        "only the foreground selection is resumed"
    );
    assert_eq!(
        runtime.active_pane().tabs().len(),
        2,
        "neither tab is closed while both runtimes are live"
    );
}

#[test]
fn a_background_exit_observed_by_scope_inventory_closes_that_tab_only() {
    let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
    let exited = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime, foreground, background) =
        foreground_and_background_panes(Box::new(BackgroundLanePort {
            log: Arc::clone(&log),
            exited: Arc::clone(&exited),
        }));
    // The bounded inventory lane observed the background shell exiting.
    exited.lock().unwrap().push(background);

    close_exited_panes(&mut ui, &mut runtime);

    let tabs = runtime.active_pane().tabs().to_vec();
    assert_eq!(tabs.len(), 1, "only the exited background tab is closed");
    assert!(
        matches!(&tabs[0], PaneTab::Live(live) if live.terminal.fences(&foreground)),
        "the foreground selection keeps streaming"
    );
    assert!(runtime.state().has_live_pane());
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "a background Agent exit must wake the coherent inventory lane"
    );
    // The closed tab stops being watched on the next frame.
    close_exited_panes(&mut ui, &mut runtime);
    assert_eq!(
        log.lock().unwrap().watched.last().cloned(),
        Some(Vec::new())
    );
}

#[test]
fn paste_markers_follow_the_focused_programs_bracketed_paste_mode() {
    for (replay, expected) in [
        (b"agent".as_slice(), b"one\ntwo".to_vec()),
        (
            b"\x1b[?2004hagent".as_slice(),
            b"\x1b[200~one\ntwo\x1b[201~".to_vec(),
        ),
        (
            b"\x1b[?2004h\x1b[?2004lagent".as_slice(),
            b"one\ntwo".to_vec(),
        ),
    ] {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(WheelRecordingPort {
                terminal,
                replay: replay.to_vec(),
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
            &Key::Paste("one\ntwo".to_owned()),
        ));
        assert_eq!(*inputs.lock().unwrap(), vec![expected]);
    }
}

#[test]
fn root_generic_host_request_is_admitted_and_untracked_resume_completion_is_inert() {
    let workspace = WorkspaceId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let mut pending = std::collections::HashMap::new();
    let (mut host, actions) = ControllerHost::channel();
    let operation = OperationId::new();
    crate::usecase::application::daemon_backend::AgentPort::open_terminal(
        &mut host,
        crate::usecase::application::daemon_backend::OpenTerminalRequest {
            target: Target::Root(workspace),
            operation_id: operation,
            arguments: "new".to_owned(),
        },
    );
    drain_host_actions(
        &actions,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending,
    );
    assert_eq!(pending.get(&operation), Some(&Target::Root(workspace)));
    assert_eq!(ui.pane_launches.len(), 1);
    assert!(
            runtime
                .panes()
                .pane(Target::Root(workspace))
                .unwrap()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Pending(pending) if pending.operation == operation && pending.kind == PaneKind::Terminal))
        );

    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::ResumeExact {
                operation: OperationId::new(),
                continuation: AgentContinuationRef::new(),
                result: Err("late answer".to_owned()),
            },
        })
        .unwrap();
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );
    assert!(
        runtime
            .panes()
            .pane(Target::Root(workspace))
            .unwrap()
            .tabs()
            .iter()
            .any(|tab| matches!(tab, PaneTab::Pending(pending) if pending.operation == operation))
    );
}

#[test]
fn unavailable_and_load_failing_intent_ports_keep_typed_fallback_state() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let mut unavailable = crate::presentation::UnavailableAgentTabIntentPort;
    assert_eq!(
        unavailable.load(workspace).unwrap(),
        AgentTabIntent::empty(workspace)
    );
    let committed = unavailable
        .mutate(
            workspace,
            0,
            AgentTabIntentMutation::Upsert {
                session_id: None,
                continuation,
                terminal,
                select: true,
            },
        )
        .unwrap();
    assert!(committed.mutation_applied);
    assert!(!committed.cas_conflict);
    assert_eq!(committed.intent.targets[0].selected, Some(continuation));

    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let observation = ui
        .observe_agent_tabs(
            Vec::new(),
            AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        )
        .unwrap();
    assert!(observation.cas_accepted);
    assert_eq!(observation.projection, AgentTabProjection::default());
    ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss { continuation })
        .unwrap();

    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_tab_intent(
        workspace,
        BTreeSet::new(),
        Box::new(LoadFailingIntentPort),
    );
    assert_eq!(
        ui.take_agent_tab_intent_load_error(),
        Some(AgentTabIntentError::ReadOnlySchema)
    );
    assert_eq!(ui.take_agent_tab_intent_load_error(), None);
}

#[test]
#[allow(clippy::too_many_lines)] // One target matrix fixes Agent ordering and generic deduplication.
fn reconciled_agent_order_precedes_deterministic_generic_inventory_per_target() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let stale_session = SessionId::new();
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let first_terminal = scoped_terminal_ref(workspace, None);
    let second_terminal = scoped_terminal_ref(workspace, None);
    let session_agent = scoped_terminal_ref(workspace, Some(session));
    let root_generic = scoped_terminal_ref(workspace, None);
    let session_generic = scoped_terminal_ref(workspace, Some(session));
    let session_generic_second = scoped_terminal_ref(workspace, Some(session));
    let stale_generic = scoped_terminal_ref(workspace, Some(stale_session));
    let projection = AgentTabProjection {
        targets: vec![
            AgentTabTargetProjection {
                session_id: None,
                tabs: vec![
                    AgentTabSlotIntent {
                        continuation: second,
                        terminal: second_terminal.clone(),
                    },
                    AgentTabSlotIntent {
                        continuation: first,
                        terminal: first_terminal.clone(),
                    },
                ],
                selected: Some(first),
            },
            AgentTabTargetProjection {
                session_id: Some(session),
                tabs: vec![AgentTabSlotIntent {
                    continuation: AgentContinuationRef::new(),
                    terminal: session_agent.clone(),
                }],
                selected: None,
            },
        ],
    };
    let entries = [
        TerminalInventoryEntry {
            terminal: session_generic.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: root_generic.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_generic_second.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_generic.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: stale_generic,
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: first_terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: first_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
    ];

    let targets = crate::presentation::pane_restore_targets(
        workspace,
        &BTreeSet::from([session]),
        projection,
        &entries,
        Some(&session_generic_second),
        Vec::new(),
        &BTreeMap::new(),
    );
    assert_eq!(targets.len(), 2);
    let root = targets
        .iter()
        .find(|target| target.target == Target::Root(workspace))
        .unwrap();
    assert_eq!(root.selected, Some(first_terminal));
    assert_eq!(root.panes[0].terminal, second_terminal);
    assert_eq!(root.panes[1].kind, PaneKind::Agent);
    assert_eq!(root.panes.len(), 3);
    assert!(
        root.panes
            .iter()
            .any(|pane| pane.kind == PaneKind::Terminal && pane.terminal.fences(&root_generic))
    );
    let managed = targets
        .iter()
        .find(|target| target.target == Target::Session(session))
        .unwrap();
    assert_eq!(managed.selected, Some(session_generic_second.clone()));
    assert_eq!(managed.panes[0].terminal, session_agent);
    assert!(
        managed
            .panes
            .iter()
            .any(|pane| pane.terminal.fences(&session_generic))
    );
    assert!(
        managed
            .panes
            .iter()
            .any(|pane| pane.terminal.fences(&session_generic_second))
    );
    assert_eq!(
        managed
            .panes
            .iter()
            .filter(|pane| pane.terminal.fences(&session_generic))
            .count(),
        1
    );
}

#[test]
fn foreground_sync_attaches_only_the_active_selected_tab() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = scoped_terminal_ref(workspace, Some(session));
    let second = scoped_terminal_ref(workspace, Some(session));
    let detaches = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_context(
        workspace,
        vec![session],
        Box::new(ScriptedAgentPort {
            terminal: first.clone(),
            subscription: 41,
            replay: b"retained".to_vec(),
            poll_error: None,
            detaches: Arc::clone(&detaches),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let (interaction, revision) = runtime.restore_fence();
    let _ = runtime.restore_snapshot(
        interaction,
        revision,
        vec![crate::presentation::PaneRestoreTarget {
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
    );
    let geometry = terminal_geometry(20, 80);

    ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
    // Re-syncing while the same selection is already attached keeps it in
    // place, exercising the fence check that avoids relaunching a live
    // foreground terminal.
    ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
    assert!(ui.terminal_rows(&first, None).is_some());
    assert!(ui.terminal_rows(&second, None).is_none());

    let _ = runtime.focus_terminal(Target::Session(session), second.clone());
    ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
    assert!(ui.terminal_rows(&first, None).is_none());
    assert!(ui.terminal_rows(&second, None).is_some());
    assert_eq!(*detaches.lock().unwrap(), vec![41]);
}

#[test]
fn stale_agent_admission_cannot_show_or_focus_a_lineage_closed_by_another_tui() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let original = scoped_terminal_ref(workspace, None);
    let replacement = scoped_terminal_ref(workspace, None);
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation,
        terminal: original.clone(),
        select: true,
    });
    initial.revision = 1;
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::new(),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations,
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let operation = OperationId::new();
    let target = Target::Root(workspace);
    let _ = runtime.request_pane(target, operation, PaneKind::Agent);
    let mut pending = std::collections::HashMap::from([(operation, target)]);

    // A second writer closes this continuation after the first TUI loaded
    // revision 1 but before its daemon admission returns.
    {
        let mut latest = durable.lock().unwrap();
        let _ = latest.apply(AgentTabIntentMutation::Dismiss { continuation });
        latest.revision += 1;
    }
    ui.pane_completion_sender
        .send(crate::presentation::PaneLaunchCompletion {
            launch_id: crate::presentation::PANE_LAUNCH_UNADMITTED,
            outcome: crate::presentation::PaneLaunchOutcome::Agent {
                operation,
                result: Ok(AgentPaneAdmission {
                    terminal: replacement,
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
        terminal_geometry(20, 80),
    );

    assert!(runtime.active_pane().tabs().is_empty());
    assert_eq!(runtime.focused_terminal(), None);
    assert!(durable.lock().unwrap().dismissed.contains(&continuation));
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&original)
    );
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::ConcurrentChange.safe_message())
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Agent and generic routing share one persistence-failure fixture.
fn persistence_failures_block_agent_reorder_and_selection_but_not_generic_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let mut intent = AgentTabIntent::empty(workspace);
    for (continuation, terminal, select) in [
        (first, first_terminal.clone(), true),
        (second, second_terminal.clone(), false),
    ] {
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal,
            select,
        });
    }
    let durable = Arc::new(Mutex::new(intent));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut command_lane = SessionCommandLane::new();
    let mut ui = io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort))
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
            selected: Some(first_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let _ = runtime.handle_key(Key::Enter);
    let tabs_before = runtime.active_pane().tabs().to_vec();
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabNext),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut std::collections::HashMap::new(),
        20,
        80,
        0,
        0,
    ));
    assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabPrevious),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut std::collections::HashMap::new(),
        20,
        80,
        0,
        0,
    ));
    assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.focused_terminal(), Some(first_terminal));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
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

    // A generic-only pane has no Agent intent to persist, so the same
    // unavailable store cannot regress its normal tab controls.
    let generic_first = scoped_terminal_ref(workspace, Some(session));
    let generic_second = scoped_terminal_ref(workspace, Some(session));
    let empty = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let generic_attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut generic_ui =
        io_runtime_on(&command_lane, view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([session]),
                Box::new(FailingIntentPort {
                    state: empty,
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&generic_attempts),
                }),
            );
    let mut generic_runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = generic_runtime.restore_fence();
    assert!(generic_runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: generic_first.clone(),
                    kind: PaneKind::Terminal,
                },
                LivePane {
                    terminal: generic_second,
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(generic_first),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let _ = generic_runtime.handle_key(Key::Enter);
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabNext),
        &mut generic_ui,
        &mut generic_runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut std::collections::HashMap::new(),
        20,
        80,
        0,
        0,
    ));
    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut generic_ui,
        &mut command_lane,
        &mut generic_runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(generic_attempts.load(Ordering::SeqCst), 0);
    assert!(generic_runtime.focused_terminal().is_some());
}

#[test]
#[allow(clippy::too_many_lines)] // The pointer boundary matrix shares one geometry fixture.
fn pointer_classifier_covers_inert_scroll_drag_and_click_boundaries() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 30,
            replay: b"hello".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    assert!(handle_terminal_pointer(
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
            kind: PointerKind::Move,
            column: 40,
            row: 5
        },
    ));
    assert!(!controls.has_selection());
    assert!(!controls.is_dragging());
    let inactive = WorkspaceRuntime::new(workspace, vec![session]);
    assert!(!forward_live_terminal_input(
        &mut ui,
        &inactive,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: Vec::new(),
        },
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: Vec::new(),
        },
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: b"fail".to_vec(),
        },
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Passthrough(b"fail".to_vec()),
    ));
    let _ = poll_and_project_terminals(
        &mut ui,
        &mut runtime,
        &mut controls,
        Geometry { cols: 43, rows: 13 },
    );

    handle_terminal_pointer(
        &ui,
        &inactive,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Drag,
            column: 40,
            row: 5,
        },
    );
    handle_terminal_pointer(
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
            kind: PointerKind::Drag,
            column: 0,
            row: 0,
        },
    );
    handle_terminal_pointer(
        &ui,
        &inactive,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Up,
            column: 40,
            row: 5,
        },
    );
    handle_terminal_pointer(
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
            kind: PointerKind::Up,
            column: 0,
            row: 0,
        },
    );
    // A focus change or an out-of-content release after a valid press
    // consumes the gesture without opening or copying.
    for (release_runtime, column, row) in [(&inactive, 40, 5), (&runtime, 0, 0)] {
        assert!(handle_terminal_pointer(
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
                column: 40,
                row: 5,
            },
        ));
        assert!(handle_terminal_pointer(
            &ui,
            release_runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Up,
                column,
                row,
            },
        ));
    }
    for column in [40, 41] {
        handle_terminal_pointer(
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
                kind: PointerKind::Drag,
                column,
                row: 5,
            },
        );
    }
    assert!(!handle_terminal_pointer(
        &ui,
        &inactive,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 40,
            row: 5,
        },
    ));
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
            column: 0,
            row: 0,
        },
    ));
    let empty_view = WorkspaceView::with_runtime_ids(ws("empty"), empty_state("empty"), vec![]);
    let empty_ui = io_runtime(empty_view, Box::new(UnavailableSessionCommandPort));
    let mut detached_controls = LiveTerminalControls::default();
    detached_controls.sync_focus(runtime.focused_terminal().as_ref());
    detached_controls.press_pointer(TerminalSelection::begin(
        vec!["detached".to_owned()],
        TerminalPoint { row: 0, column: 0 },
    ));
    assert!(handle_terminal_pointer(
        &empty_ui,
        &runtime,
        &mut detached_controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Up,
            column: 40,
            row: 5,
        },
    ));
    assert!(!handle_terminal_pointer(
        &empty_ui,
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
            column: 40,
            row: 5,
        },
    ));
    let mut empty_controls = LiveTerminalControls::default();
    for kind in [PointerKind::Drag, PointerKind::Up] {
        handle_terminal_pointer(
            &empty_ui,
            &runtime,
            &mut empty_controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind,
                column: 40,
                row: 5,
            },
        );
    }

    let mut pending = std::collections::HashMap::new();
    for key in [
        Key::Live(LiveTerminalAction::ScrollUp),
        Key::Live(LiveTerminalAction::ScrollDown),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Drag,
            column: 0,
            row: 0,
        }),
        Key::Click { column: 0, row: 0 },
    ] {
        let _ = intercept_live_terminal_control(
            &key,
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            1,
            0,
        );
    }
}

#[test]
fn agent_reply_auto_scroll_moves_the_retained_highlight_with_its_text() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let replay = b"\x1b[?1049hheader\x1b[13;1Hcomposer\x1b[2;3r\x1b[2;1Hone\r\ntwo".to_vec();
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScrollingAgentPort {
            terminal: terminal.clone(),
            replay,
            output: Some(b"\x1b[1S\x1b[3;1Hreply".to_vec()),
        }),
    );
    let geometry = terminal_geometry(20, 80);
    let mut controls = LiveTerminalControls::default();
    // Drain the attach replacement before the user starts selecting, then
    // establish the alternate-buffer coordinate space from the first view.
    crate::presentation::sync_terminal_selection_motions(&mut ui, &mut controls);
    let _ = controller_terminal_view(&ui, &runtime, &mut controls, usize::from(geometry.rows));
    let mut selection = ui
        .begin_terminal_selection(&terminal, TerminalPoint { row: 2, column: 0 })
        .expect("attached Agent screen");
    selection.extend(TerminalPoint { row: 2, column: 2 });
    controls.begin_selection(selection);
    assert_eq!(controls.finish_drag().as_deref(), Some("two"));

    let (view, _, _) = poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let rows = view.expect("live Agent view").rows;

    assert!(
        rows[1].contains("\x1b[7mtwo"),
        "Agent reply left the highlight at its old screen row: {rows:?}"
    );
    assert!(rows[2].contains("reply"));
    assert_eq!(
        controls.selection().map(TerminalSelection::text).as_deref(),
        Some("two")
    );
}

#[test]
fn idle_agent_port_is_safe_when_an_unexpected_launch_is_requested() {
    let mut port = IdleAgentPort;
    let error = port
        .launch(
            OperationId::new(),
            WorkspaceId::new(),
            Some(SessionId::new()),
            None,
        )
        .unwrap_err();

    assert_eq!(error, "not launched in this test");
    assert_eq!(
        port.launch_terminal(
            WorkspaceId::new(),
            Some(SessionId::new()),
            Geometry { cols: 80, rows: 24 },
            "open",
            OperationId::new(),
        )
        .unwrap_err(),
        "terminal launch is unavailable"
    );
}

#[test]
fn interruptible_loading_defers_non_escape_input_for_the_next_screen() {
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::CtrlQ]),
        ..ResponsiveLoadingTerminal::default()
    };
    let draw_count = Arc::clone(&term.draw_count);

    run_workspace_loading(&mut term, "Opening workspace…", true, || {
        while draw_count.load(std::sync::atomic::Ordering::Acquire) < 2 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    })
    .expect("a non-cancelling key does not interrupt the operation");

    assert_eq!(term.read_key().unwrap(), Key::CtrlQ);
}

#[test]
fn prepared_existing_tab_uses_the_cached_switch_surface() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
    let mut term = ResponsiveLoadingTerminal::default();
    let mut loader = FakeLoader {
        operation_mode: FakeOperationMode::Background,
        open_delay: std::time::Duration::from_millis(8),
        ..FakeLoader::default()
    };
    let mut loader_port: Option<&mut dyn WorkspaceLoader> = Some(&mut loader);

    let prepared = prepare_deck_workspace(
        &mut term,
        &mut loader_port,
        &mut deck,
        &beta.workspace.path,
        "Opening workspace…",
    );

    assert!(prepared.is_some());
    assert!(!term.frames.is_empty());
    assert!(
        term.frames
            .iter()
            .all(|frame| frame.join("\n").contains("beta-session"))
    );
}

#[test]
fn cancelled_switch_never_replaces_the_deck_with_a_full_screen_loader() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::Escape]),
        ..ResponsiveLoadingTerminal::default()
    };
    let mut loader = FakeLoader {
        operation_mode: FakeOperationMode::Background,
        open_delay: std::time::Duration::from_millis(8),
        ..FakeLoader::default()
    };
    let mut loader_port: Option<&mut dyn WorkspaceLoader> = Some(&mut loader);

    assert!(
        prepare_deck_workspace(
            &mut term,
            &mut loader_port,
            &mut deck,
            &beta.workspace.path,
            "Opening workspace…",
        )
        .is_none()
    );

    assert!(term.frames.iter().all(|frame| {
        let frame = frame.join("\n");
        frame.contains("1 alpha")
            && frame.contains("2 beta")
            && (frame.contains("alpha-session") || frame.contains("beta-session"))
    }));
}

#[test]
fn a_panicking_background_operation_is_reported_as_io_failure() {
    let mut term = ResponsiveLoadingTerminal::default();
    let error = run_workspace_loading(
        &mut term,
        "Saving settings…",
        false,
        || -> io::Result<()> { panic!("injected worker panic") },
    )
    .expect_err("worker panic is mapped to a stable error");

    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(
        error.to_string(),
        "background operation stopped unexpectedly"
    );
}

#[test]
fn responsive_save_helpers_cover_empty_and_inline_environment_requests() {
    let mut background = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut clean = Config::load(&mut background);
    let mut term = ResponsiveLoadingTerminal::default();
    assert!(!save_config_responsive(&mut term, &mut clean, &mut background, None).unwrap());
    assert!(!save_environment_responsive(
        &mut term,
        &mut clean,
        &mut background
    ));
    assert!(!save_setup_commands_responsive(
        &mut term,
        &mut clean,
        &mut background
    ));

    let mut inline = RecordingSettingsPort::default();
    let mut environment = Config::load(&mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Enter, &mut inline);
    let _ = step_config(
        &mut environment,
        Key::Paste("INLINE=1".to_owned()),
        &mut inline,
    );
    let _ = step_config(&mut environment, Key::Tab, &mut inline);
    assert!(save_environment_responsive(
        &mut term,
        &mut environment,
        &mut inline
    ));
    assert_eq!(inline.environment_saves, 1);

    let mut setup =
        Config::load_workspace_with_available_models(&mut inline, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut setup, Key::Down, &mut inline);
    }
    let _ = step_config(&mut setup, Key::Enter, &mut inline);
    let _ = step_config(&mut setup, Key::Paste("cargo test".to_owned()), &mut inline);
    assert!(save_setup_commands_responsive(
        &mut term,
        &mut setup,
        &mut inline,
    ));
    assert_eq!(inline.setup_commands, ["cargo test"]);
}

#[test]
fn failed_clone_retains_every_clone_draft_field_and_mode() {
    let mut keys = vec![Key::Char('e'), Key::Down];
    keys.extend("https://example.com/acme/app.git".chars().map(Key::Char));
    keys.push(Key::Down);
    keys.extend("/tmp".chars().map(Key::Char));
    keys.push(Key::Down); // derived directory `app`
    keys.push(Key::Down);
    keys.extend("feature".chars().map(Key::Char));
    keys.extend([Key::Enter, Key::Quit]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(
        loader.created,
        [NewRequest::Clone {
            repository: "https://example.com/acme/app.git".to_owned(),
            destination: PathBuf::from("/tmp/app"),
            branch: Some("feature".to_owned()),
        }]
    );
    let failed = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("open failed"))
        .expect("failed Clone frame");
    let failed = crate::presentation::widgets::strip_ansi(&failed.join("\n"));
    for value in [
        "Clone",
        "https://example.com/acme/app.git",
        "/tmp",
        "app",
        "feature",
    ] {
        assert!(failed.contains(value), "missing {value}: {failed}");
    }
}

#[test]
fn missing_recent_can_cancel_without_mutating_the_registry() {
    let alpha = ws("alpha");
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('n'), Key::Quit]);
    let mut loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };

    assert_eq!(
        run(
            &mut term,
            vec![alpha],
            vec![recent("alpha")],
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.cleanup_calls, 0);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Workspace not found"))
    );
    assert!(term.frames.last().unwrap().join("\n").contains("alpha"));

    let alpha = ws("alpha");
    let mut confirm_term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('y'), Key::Quit]);
    let mut confirm_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut confirm_term,
        vec![alpha],
        vec![recent("alpha")],
        now(),
        &mut confirm_loader,
    )
    .unwrap();
    let final_frame = confirm_term.frames.last().unwrap().join("\n");
    assert!(final_frame.contains("No recent workspace"));
    assert!(!final_frame.contains("alpha"));
}

#[test]
fn unreadable_recent_reports_the_error_without_a_removal_prompt() {
    let alpha = ws("alpha");
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
    let mut loader = FakeLoader {
        missing_error: Some(io::ErrorKind::PermissionDenied),
        ..FakeLoader::default()
    };

    run(
        &mut term,
        vec![alpha],
        vec![recent("alpha")],
        now(),
        &mut loader,
    )
    .unwrap();

    assert_eq!(loader.cleanup_calls, 0);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("workspace path could not be checked"))
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame.contains("Workspace not found"))
    );
}

#[test]
fn key_help_scroll_keys_drive_the_bounded_viewport() {
    use crate::presentation::views::key_help::{Context, State};

    let initial = State::new(
        Context::Switch,
        usagi_core::domain::settings::WorkMode::GoalDriven,
    );
    let mut state = initial;

    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::Up,
        5
    ));
    assert_eq!(state, initial);
    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::Down,
        5
    ));
    assert_ne!(state, initial);
    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::Home,
        5
    ));
    assert_eq!(state, initial);
    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::PageDown,
        20
    ));
    assert_ne!(state, initial);
    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::PageUp,
        20
    ));
    assert_eq!(state, initial);
    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::End,
        20
    ));
    assert_ne!(state, initial);
    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::LineStart,
        20
    ));
    assert_eq!(state, initial);
    assert!(crate::presentation::scroll_key_help(
        &mut state,
        &Key::LineEnd,
        20
    ));
    assert_ne!(state, initial);
    assert!(!crate::presentation::scroll_key_help(
        &mut state,
        &Key::Other,
        20
    ));
}

#[test]
fn public_value_derives_are_exercised() {
    let snapshot = snapshot("derive");
    assert_eq!(snapshot.clone(), snapshot);
    assert!(format!("{snapshot:?}").contains("derive"));
    let quit = Exit::Quit;
    assert_eq!(quit, Exit::Quit);
    assert!(format!("{quit:?}").contains("Quit"));
    let welcome = Exit::Welcome;
    assert_eq!(welcome, Exit::Welcome);
    assert_ne!(welcome, quit);
    assert!(format!("{welcome:?}").contains("Welcome"));
}

#[test]
fn doctor_runner_requires_a_report() {
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::new(&mut buf, &info);
    assert_eq!(
        dispatch(&EntryScreen::Doctor, &mut runner)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn selecting_a_resumable_interrupted_tab_starts_its_exact_resume() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests,
        })),
    );

    let mut pending_targets = std::collections::HashMap::new();
    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);

    assert_eq!(ui.pane_launches.len(), 1);
    assert!(matches!(
        ui.pane_launches.first(),
        Some(PaneLaunch::ResumeExact { .. })
    ));
}

#[test]
fn managed_host_tab_cycle_selects_pending_and_activates_interrupted_history() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let continuation = history.continuation;
    let last_terminal = history.last_terminal.clone();
    let mut command_lane = SessionCommandLane::new();
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        Box::new(UnavailablePaneLaunchPort),
    );
    ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: last_terminal,
        select: true,
    })
    .unwrap();
    let operation = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: operation,
        profile: None,
    });
    let mut pending_targets = std::collections::HashMap::new();

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending_targets,
    );
    assert_eq!(
        runtime.active_pane().selected(),
        &PaneSelection::Tab(TabSelection::Pending(operation))
    );

    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Previous))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut command_lane,
        &mut runtime,
        &mut pending_targets,
    );
    assert_eq!(
        runtime.focused_interrupted().map(|tab| tab.continuation),
        Some(continuation)
    );
    assert_eq!(
        runtime
            .interrupted_removal_confirmation()
            .map(|prompt| prompt.tab().continuation),
        Some(continuation)
    );
}

#[test]
fn selecting_an_unresumable_tab_prompts_then_keeps_or_removes_exact_history() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let continuation = history.continuation;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending_targets = std::collections::HashMap::new();

    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    let prompt = runtime
        .interrupted_removal_confirmation()
        .expect("an unresumable selection opens its removal prompt");
    assert_eq!(prompt.tab().continuation, continuation);
    assert!(!prompt.is_confirm_selected());
    let frame = render_home_material(&home_frame_material(
        20,
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
    ))
    .join("\n");
    assert!(frame.contains("Remove interrupted Agent"));
    assert!(frame.contains("Enter: select"));
    assert!(frame.contains("y: remove"));
    assert!(frame.contains("Esc/n: keep"));
    assert!(frame.contains("[ remove"));
    assert!(frame.contains("[ keep   ]"));
    assert!(frame.contains("kept no resume metadata"));

    // The prompt owns all input. A terminal EOF cannot leak to a pane
    // behind it, and choosing Keep leaves both durable and local history.
    assert!(handle_interrupted_removal_confirmation(
        &Key::CtrlD,
        &mut ui,
        &mut runtime,
    ));
    assert!(runtime.interrupted_removal_confirmation().is_some());
    assert!(handle_interrupted_removal_confirmation(
        &Key::Enter,
        &mut ui,
        &mut runtime,
    ));
    assert!(runtime.interrupted_removal_confirmation().is_none());
    assert!(runtime.active_pane().has_tabs());
    assert!(ui.agent_dismissed().is_empty());

    // Escape is the explicit Keep shortcut and also leaves history intact.
    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    assert!(handle_interrupted_removal_confirmation(
        &Key::Escape,
        &mut ui,
        &mut runtime,
    ));
    assert!(runtime.interrupted_removal_confirmation().is_none());
    assert!(runtime.active_pane().has_tabs());

    // Selecting the same tab again reopens a fresh safe-default prompt.
    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    assert!(handle_interrupted_removal_confirmation(
        &Key::Left,
        &mut ui,
        &mut runtime,
    ));
    assert!(handle_interrupted_removal_confirmation(
        &Key::Enter,
        &mut ui,
        &mut runtime,
    ));
    assert!(!runtime.active_pane().has_tabs());
    assert_eq!(ui.agent_dismissed(), BTreeSet::from([continuation]));
    assert!(requests.lock().unwrap().is_empty());
    assert!(ui.pane_launches.is_empty());
    crate::presentation::confirm_interrupted_removal(&mut ui, &mut runtime);
}

#[test]
fn unresumable_prompt_y_shortcut_removes_the_exact_history() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    // `y` remains a direct, explicit Remove shortcut independent of focus.
    let y_history = interrupted_history(workspace, Some(session), false);
    let y_continuation = y_history.continuation;
    let (mut y_ui, mut y_runtime) = closeup_with_history(
        workspace,
        session,
        vec![y_history],
        Box::new(UnavailablePaneLaunchPort),
    );
    assert!(select_right_pane_tab(&mut y_ui, &mut y_runtime, 0));
    activate_focused_interrupted_tab(
        &mut y_ui,
        &mut y_runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(handle_interrupted_removal_confirmation(
        &Key::Char('y'),
        &mut y_ui,
        &mut y_runtime,
    ));
    assert!(!y_runtime.active_pane().has_tabs());
    assert_eq!(y_ui.agent_dismissed(), BTreeSet::from([y_continuation]));
}

#[test]
fn a_refused_or_failed_resume_keeps_the_history_tab_with_safe_feedback() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let mut relationless = exact_resume_answer(&history);
    relationless.relation = None;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![
                Err("provider resume failed; refresh Agent inventory".to_owned()),
                Ok(relationless),
            ],
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    for _ in 0..2 {
        crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        std::thread::sleep(std::time::Duration::from_millis(20));
        crate::presentation::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );
        // The tab survives every refusal, and no live pane is invented.
        assert_eq!(runtime.active_pane().tabs().len(), 1);
        assert!(matches!(
            runtime.active_pane().tabs()[0],
            PaneTab::Interrupted(_)
        ));
        assert!(runtime.focused_terminal().is_none());
        assert!(runtime.active_pane().error().is_some());
    }
    // A transport failure and a relation-less answer are both retryable, so
    // both requests reached the daemon.
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[test]
fn an_unresumable_history_tab_never_reaches_the_daemon() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert!(ui.pane_launches.is_empty());
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(
        runtime.active_pane().error(),
        Some(
            crate::usecase::application::interrupted_tab::ResumeRejection::NotResumable
                .safe_message()
        )
    );
}

#[test]
fn closing_an_inventory_only_history_tab_persists_its_removal_without_resuming() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history.clone()],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    assert!(ui.agent_slot_order().is_empty());
    assert_eq!(
        runtime.focused_interrupted().map(|tab| tab.continuation),
        Some(history.continuation),
        "an interrupted-only pane must be selectable without opening another Agent"
    );

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
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

    assert!(!runtime.active_pane().has_tabs());
    assert_eq!(ui.agent_slot_order(), vec![history.continuation]);
    assert_eq!(ui.agent_dismissed(), BTreeSet::from([history.continuation]));
    assert!(runtime.active_pane().error().is_none());
    assert!(runtime.state().notice().is_none());
    assert!(requests.lock().unwrap().is_empty());
    assert!(ui.pane_launches.is_empty());

    // A later coherent inventory replay receives the durable dismissal and
    // therefore cannot resurrect the tab that Ctrl-O x removed.
    let inventory = AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(
                history.target.as_ref().unwrap().runtime_id,
                history.last_terminal.clone(),
                Some(session),
            )
            .unwrap(),
            continuation: history.continuation,
            state: AgentRuntimeInventoryState::Interrupted,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    assert!(
        crate::usecase::application::interrupted_tab::project(
            &inventory,
            workspace,
            &BTreeSet::from([session]),
            &ui.agent_slot_order(),
            &ui.agent_dismissed(),
            &BTreeSet::new(),
        )
        .tabs
        .is_empty()
    );
}

#[test]
fn a_queued_resume_waits_for_the_daemon_port_and_never_duplicates_its_request() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = interrupted_history(workspace, Some(session), true);
    let second = interrupted_history(workspace, Some(session), true);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![first.clone(), second.clone()],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![
                Ok(exact_resume_answer(&first)),
                Ok(exact_resume_answer(&second)),
            ],
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    // Resume both history tabs before either answer arrives.
    crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    let _ = runtime.select_tab(TabDirection::Next);
    crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert_eq!(ui.pane_launches.len(), 2);

    // Only one worker may own the stateful daemon port: the second request
    // stays queued instead of starting a second concurrent resume.
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(ui.pane_launches.len(), 1);
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(ui.pane_launches.len(), 1);
    await_requests(&requests, 1);

    // Once the port returns with the first answer the queued one runs.
    for _ in 0..2 {
        crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        std::thread::sleep(std::time::Duration::from_millis(20));
        crate::presentation::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );
    }
    await_requests(&requests, 2);
    assert!(
        runtime
            .active_pane()
            .tabs()
            .iter()
            .all(|tab| matches!(tab, PaneTab::Live(_)))
    );
}

#[test]
fn a_resume_without_an_agent_context_or_a_selected_history_tab_does_nothing() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut pending = std::collections::HashMap::new();

    // No daemon Agent context at all.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut bare = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    crate::presentation::resume_focused_interrupted_tab(&mut bare, &mut runtime, &mut pending);
    assert!(bare.pane_launches.is_empty());

    // An Agent context with no active managed target stops at the runtime
    // target boundary before looking for an interrupted tab.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut inactive = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    crate::presentation::resume_focused_interrupted_tab(&mut inactive, &mut runtime, &mut pending);
    assert!(inactive.pane_launches.is_empty());

    // An Agent context whose selected tab is live, not interrupted.
    let history = interrupted_history(workspace, Some(session), true);
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::new(Mutex::new(Vec::new())),
        })),
    );
    let live = live_terminal_ref(workspace, session);
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
    let _ = runtime.complete_pane(Target::Session(session), operation, live.clone());
    let _ = runtime.focus_terminal(Target::Session(session), live);
    crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert!(ui.pane_launches.is_empty());
}

#[test]
fn an_accepted_resume_whose_display_intent_cannot_be_saved_surfaces_a_typed_notice() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let answer = exact_resume_answer(&history);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_pane_launch_port(launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![Ok(answer)],
            requests: Arc::new(Mutex::new(Vec::new())),
        })))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Down);
    let _ = runtime.handle_key(Key::Enter);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        interaction,
        revision,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Session(session),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: None,
            interrupted: vec![history],
        }],
    ));
    let _ = runtime.select_tab(TabDirection::Next);
    let mut pending = std::collections::HashMap::new();

    crate::presentation::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    crate::presentation::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::presentation::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    // A daemon success is not shown as committed until display intent is
    // durable. The interrupted tab stays in place and the typed failure is
    // visible, so neither pane state nor intent bytes claims success.
    assert!(runtime.focused_terminal().is_none());
    assert!(runtime.focused_interrupted().is_some());
    assert!(runtime.state().notice().is_some());
}

/// A daemon that cannot be reached must not end the process from an entry
/// screen. It is the same shape of failure as a workspace this daemon does
/// not serve — the switcher stays up with the reason — and collapsing to the
/// shell here is exactly how a wedged daemon locks a user out of usagi.
#[test]
fn an_unreachable_daemon_keeps_the_switcher_up_instead_of_ending_the_process() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('q'), Key::Enter]);
    let mut loader = FakeLoader {
        unreachable: Some("daemon unavailable: the daemon did not answer".to_owned()),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            Vec::new(),
            vec![recent_at("first", now())],
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

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/first")]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.iter().any(|line| line.contains("did not answer"))),
        "the switcher must show why the workspace did not open"
    );
    // The outage happened before any workspace runtime existed, so no daemon
    // port was created for a workspace that never opened.
    assert_eq!(factory.drops.load(Ordering::SeqCst), 0);
}

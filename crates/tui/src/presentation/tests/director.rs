//! director の presentation 振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;
use crate::presentation::{
    DirectorConversation, DirectorDrawerProjection, DirectorOrganizationRow,
    complete_director_launch, widgets,
};

fn launch_fixture(
    failure: Option<AgentTabIntentError>,
) -> (WorkspaceIoRuntime, WorkspaceRuntime, TerminalRef) {
    let workspace = WorkspaceId::new();
    let old = scoped_terminal_ref(workspace, None);
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation: AgentContinuationRef::new(),
        terminal: old.clone(),
        select: true,
    });
    let durable = Arc::new(Mutex::new(intent));
    let port: Box<dyn AgentTabIntentPort> = if let Some(error) = failure {
        Box::new(FailingIntentPort {
            state: durable,
            error,
            attempts: Arc::new(AtomicUsize::new(0)),
        })
    } else {
        Box::new(MemoryIntentPort {
            state: durable,
            mutations: Arc::new(Mutex::new(Vec::new())),
        })
    };
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
        .with_agent_tab_intent(workspace, BTreeSet::new(), port);
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: vec![LivePane {
                terminal: old.clone(),
                kind: PaneKind::Agent
            }],
            selected: Some(old.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }]
    ));
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    (ui, runtime, old)
}

fn start_conversation(runtime: &mut WorkspaceRuntime) -> OperationId {
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
    let effects = runtime.handle_key(Key::Enter);
    let [effect @ Effect::LaunchAgent { operation_id, .. }] = effects.as_slice() else {
        panic!("launch expected")
    };
    runtime.on_effect(effect);
    *operation_id
}

fn finish_conversation(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    operation: OperationId,
    result: Result<AgentPaneAdmission, String>,
) {
    let target = Target::Root(runtime.state().workspace());
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    crate::presentation::apply_agent_launch_completion(
        ui,
        runtime,
        &mut pending,
        operation,
        result,
    );
    assert!(pending.is_empty());
}

fn new_conversation(old: &TerminalRef) -> AgentPaneAdmission {
    AgentPaneAdmission {
        terminal: TerminalRef {
            terminal_id: TerminalId::new(),
            ..old.clone()
        },
        continuation: Some(AgentContinuationRef::new()),
    }
}

#[test]
fn failed_intent_commit_preserves_director_route_and_conversation() {
    for error in [
        AgentTabIntentError::Unavailable,
        AgentTabIntentError::ConcurrentChange,
        AgentTabIntentError::ReadOnlySchema,
        AgentTabIntentError::InvalidMutation,
    ] {
        for route in [DirectorRoute::Organization, DirectorRoute::Console] {
            let (mut ui, mut runtime, old) = launch_fixture(Some(error));
            let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
            if route == DirectorRoute::Console {
                let _ = runtime.handle_key(Key::Enter);
            }
            let before = ui.agent_tab_intent.as_ref().unwrap().state.clone();
            let operation = start_conversation(&mut runtime);
            finish_conversation(&mut ui, &mut runtime, operation, Ok(new_conversation(&old)));
            assert_eq!(runtime.director_terminal(), Some(old));
            assert_eq!(runtime.state().director_launching(), None);
            assert!(runtime.active_pane().error().is_some());
            assert_eq!(runtime.state().director_route(), route);
            assert_eq!(ui.agent_tab_intent.as_ref().unwrap().state, before);
            assert_eq!(runtime.active_pane().tabs().len(), 1);
        }
    }
}

#[test]
fn starting_keys_do_not_prevent_selecting_the_successfully_created_conversation() {
    for key in [
        None,
        Some(Key::Enter),
        Some(Key::Escape),
        Some(Key::Down),
        Some(Key::Up),
        Some(Key::Live(LiveTerminalAction::NextTab)),
        Some(Key::Live(LiveTerminalAction::PreviousTab)),
        Some(Key::Live(LiveTerminalAction::DirectorBack)),
        Some(Key::Live(LiveTerminalAction::DirectorNew)),
    ] {
        let (mut ui, mut runtime, old) = launch_fixture(None);
        let operation = start_conversation(&mut runtime);
        if let Some(key) = key {
            assert_eq!(
                crate::presentation::handle_director_picker_input(&mut runtime, &key),
                Some(Vec::new())
            );
        }
        let admission = new_conversation(&old);
        let new = admission.terminal.clone();
        let continuation = admission.continuation;
        finish_conversation(&mut ui, &mut runtime, operation, Ok(admission));
        assert_eq!(
            ui.agent_tab_intent.as_ref().unwrap().state.targets[0].selected,
            continuation
        );
        assert_eq!(runtime.state().director_route(), DirectorRoute::Console);
        assert_eq!(runtime.director_terminal(), Some(new.clone()));
        assert_eq!(runtime.focused_terminal(), Some(new));
        assert!(runtime.wants_live_input());
        assert_eq!(runtime.active_pane().tabs().len(), 2);
        // Duplicate completion must not add another tab or select a new identity.
        let before = ui.agent_tab_intent.as_ref().unwrap().state.clone();
        crate::presentation::apply_agent_launch_completion(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
            operation,
            Ok(new_conversation(&old)),
        );
        assert_eq!(ui.agent_tab_intent.as_ref().unwrap().state, before);
        assert_eq!(runtime.active_pane().tabs().len(), 2);
    }
}

#[test]
fn root_conversation_completion_preserves_shell_focus_and_selects_the_new_agent() {
    let (mut ui, mut runtime, old) = launch_fixture(None);
    let target = Target::Root(old.workspace_id);
    let shell_operation = OperationId::new();
    let shell = new_conversation(&old).terminal;
    let _ = runtime.request_pane(target, shell_operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(target, shell_operation, shell.clone());
    let operation = start_conversation(&mut runtime);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(runtime.focused_terminal(), Some(shell.clone()));
    let admission = new_conversation(&old);
    let new = admission.terminal.clone();
    finish_conversation(&mut ui, &mut runtime, operation, Ok(admission));
    assert_eq!(runtime.focused_terminal(), Some(shell.clone()));
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert_eq!(runtime.director_terminal(), Some(new.clone()));
    assert_eq!(runtime.root_terminal(), Some(shell));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(runtime.focused_terminal(), Some(new));
}

#[test]
fn root_conversation_completion_keeps_the_drawer_closed_until_reopened() {
    let (mut ui, mut runtime, old) = launch_fixture(None);
    let operation = start_conversation(&mut runtime);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(!runtime.state().director_drawer_open());
    let admission = new_conversation(&old);
    let new = admission.terminal.clone();
    finish_conversation(&mut ui, &mut runtime, operation, Ok(admission));
    assert!(!runtime.state().director_drawer_open());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.director_terminal(), Some(new));
}

#[test]
fn rejected_root_admissions_preserve_route_selection_and_saved_intent() {
    for rejection in 0..5 {
        let (mut ui, mut runtime, old) = launch_fixture(None);
        let operation = start_conversation(&mut runtime);
        let before = ui.agent_tab_intent.as_ref().unwrap().state.clone();
        let mut admission = new_conversation(&old);
        match rejection {
            0 => admission.continuation = None,
            1 => admission.terminal.workspace_id = WorkspaceId::new(),
            2 => admission.terminal.session_id = Some(SessionId::new()),
            3 => {
                let _ = runtime.fail_pane(
                    Target::Root(old.workspace_id),
                    operation,
                    "cancelled".into(),
                );
            }
            _ => {}
        }
        let result = if rejection == 4 {
            Err("launch failed".into())
        } else {
            Ok(admission)
        };
        finish_conversation(&mut ui, &mut runtime, operation, result);
        assert_eq!(
            runtime.state().director_route(),
            DirectorRoute::Organization
        );
        assert_eq!(runtime.state().director_launching(), None);
        assert_eq!(runtime.director_terminal(), Some(old));
        assert_eq!(ui.agent_tab_intent.as_ref().unwrap().state, before);
        assert_eq!(runtime.active_pane().tabs().len(), 1);
    }
}

#[test]
fn organization_status_prioritizes_session_lifecycle_over_agent_activity() {
    use crate::usecase::application::controller::SessionRoleProjection;
    use usagi_core::domain::agent::AgentStatus;
    use usagi_core::domain::session_lifecycle::SessionLifecycle;
    use usagi_core::domain::session_lifecycle::SessionLifecycleProjection;
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let mut state = state("demo");
    state.sessions.truncate(1);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state, vec![session]);
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let agents = [
        (None, "ready"),
        (Some(AgentStatus::Starting), "starting"),
        (Some(AgentStatus::Running), "running"),
        (Some(AgentStatus::Idle), "waiting"),
        (Some(AgentStatus::Exited), "stopped"),
        (Some(AgentStatus::Failed), "failed"),
    ];
    for (agent, available_status) in agents {
        for (lifecycle, expected) in [
            (None, available_status),
            (Some(SessionLifecycle::Available), available_status),
            (Some(SessionLifecycle::Failed), "failed"),
            (Some(SessionLifecycle::Deleting), "deleting"),
            (Some(SessionLifecycle::Creating), "creating"),
            (Some(SessionLifecycle::Initializing), "initializing"),
        ] {
            ui.workspace.set_session_roles(BTreeMap::from([(
                session,
                SessionRoleProjection {
                    creator_continuation: Some(continuation),
                    director_continuation: Some(continuation),
                    parent_session_id: None,
                    role_id: None,
                    role_summary: None,
                    agent_status: agent,
                },
            )]));
            ui.workspace.set_session_lifecycles(
                lifecycle
                    .map(|lifecycle| {
                        (
                            session,
                            SessionLifecycleProjection {
                                lifecycle,
                                failure_stage: None,
                                failure_summary: None,
                            },
                        )
                    })
                    .into_iter()
                    .collect(),
            );
            let rows = director_organization(&ui, Some(continuation));
            assert_eq!(rows[1].status, expected);
        }
    }
}

#[test]
fn failed_session_is_reported_failed_in_organization() {
    use crate::usecase::application::controller::SessionRoleProjection;
    use usagi_core::domain::session_lifecycle::{
        FailureStage, SessionLifecycle, SessionLifecycleProjection,
    };
    let continuation = AgentContinuationRef::new();
    let session = SessionId::new();
    let mut workspace_state = state("demo");
    workspace_state.sessions.truncate(1);
    workspace_state.sessions[0].name = "failed-worker".into();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), workspace_state, vec![session]);
    view.set_session_roles(BTreeMap::from([(
        session,
        SessionRoleProjection {
            creator_continuation: Some(continuation),
            director_continuation: Some(continuation),
            parent_session_id: None,
            role_id: None,
            role_summary: None,
            agent_status: None,
        },
    )]));
    view.set_session_lifecycles(BTreeMap::from([(
        session,
        SessionLifecycleProjection {
            lifecycle: SessionLifecycle::Failed,
            failure_stage: Some(FailureStage::Initialize),
            failure_summary: Some("setup failed".into()),
        },
    )]));
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let rows = director_organization(&ui, Some(continuation));
    assert_eq!(rows.len(), 2);
    assert!(rows[1].label.contains("failed-worker"));
    assert_eq!(rows[1].status, "failed");
}

#[test]
fn starting_from_organization_consumes_navigation_and_preserves_failure_route() {
    for key in [
        Key::Enter,
        Key::Down,
        Key::Up,
        Key::Live(LiveTerminalAction::NextTab),
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Live(LiveTerminalAction::ScrollDown),
    ] {
        let workspace = WorkspaceId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::Claude]),
            DefaultModel::Claude,
        );
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
        let effects = runtime.handle_key(Key::Enter);
        let [effect @ Effect::LaunchAgent { operation_id, .. }] = effects.as_slice() else {
            panic!("missing launch")
        };
        runtime.on_effect(effect);
        let selection = runtime.director_selection();
        assert_eq!(
            crate::presentation::handle_director_picker_input(&mut runtime, &key),
            Some(Vec::new()),
            "{key:?}"
        );
        assert_eq!(
            runtime.state().director_route(),
            DirectorRoute::Organization
        );
        assert_eq!(runtime.director_selection(), selection);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
        let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
        let _ = crate::presentation::select_director_tab(&key, &mut ui, &mut runtime);
        assert_eq!(runtime.director_selection(), selection);
        complete_director_launch(&mut runtime, Target::Root(workspace), *operation_id, false);
        assert_eq!(
            runtime.state().director_route(),
            DirectorRoute::Organization
        );
    }
}

#[test]
fn director_organization_keeps_orphans_with_saved_root_membership() {
    use crate::usecase::application::controller::SessionRoleProjection;
    let director = AgentContinuationRef::new();
    let worker_conversation = AgentContinuationRef::new();
    let child = SessionId::new();
    let mut workspace_state = state("demo");
    workspace_state.sessions.truncate(1);
    workspace_state.sessions[0].name = "surviving-worker".into();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), workspace_state, vec![child]);
    view.set_session_roles(BTreeMap::from([(
        child,
        SessionRoleProjection {
            creator_continuation: Some(worker_conversation),
            director_continuation: Some(director),
            parent_session_id: Some(SessionId::new()),
            role_id: None,
            role_summary: None,
            agent_status: None,
        },
    )]));
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let rows = director_organization(&ui, Some(director));
    assert_eq!(rows.len(), 2);
    assert!(rows[1].label.contains("surviving-worker"));
    assert_eq!(rows[1].depth, 1);
    assert_eq!(
        director_organization(&ui, Some(AgentContinuationRef::new())).len(),
        1
    );
}

#[test]
fn organization_tree_scroll_reaches_every_row_and_resets_for_other_conversations() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let first = interrupted_history(workspace, None, true);
    let second = interrupted_history(workspace, None, true);
    let second_id = second.continuation;
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: Some(first.continuation),
            interrupted: vec![first, second],
        }]
    ));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let mut projection = DirectorDrawerProjection {
        conversations: vec![DirectorConversation {
            label: "Conversation".into(),
            selected: true,
        }],
        organization: (0..=20)
            .map(|i| DirectorOrganizationRow {
                depth: usize::from(i != 0),
                label: format!("worker-{i:02}"),
                status: "running".into(),
            })
            .collect(),
        ..DirectorDrawerProjection::default()
    };
    for height in [13, 24, 40] {
        let _ = runtime.apply_event(AppEvent::Resize { width: 100, height });
        runtime.scroll_director_organization(false, usize::MAX);
        for i in 0..=20 {
            projection.organization_cursor = runtime.director_organization_cursor();
            runtime.set_director_projection(projection.clone());
            let text = widgets::strip_ansi(
                &director_drawer::render_over(usize::from(height), 100, &[], &projection)
                    .join("\n"),
            );
            assert!(text.contains(&format!("worker-{i:02}")), "{height}: {text}");
            assert_eq!(
                crate::presentation::handle_director_picker_input(
                    &mut runtime,
                    &Key::Live(LiveTerminalAction::ScrollDown)
                ),
                Some(Vec::new())
            );
        }
        assert_eq!(runtime.director_organization_cursor(), 20);
    }
    let _ = crate::presentation::handle_director_picker_input(
        &mut runtime,
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 90,
            row: 15,
            notches: 4,
        }),
    );
    assert_eq!(runtime.director_organization_cursor(), 16);
    let _ = crate::presentation::handle_director_picker_input(
        &mut runtime,
        &Key::Live(LiveTerminalAction::ScrollUp),
    );
    assert_eq!(runtime.director_organization_cursor(), 15);
    let _ = crate::presentation::handle_director_picker_input(
        &mut runtime,
        &Key::Live(LiveTerminalAction::ScrollBottom),
    );
    assert_eq!(runtime.director_organization_cursor(), 20);
    projection.organization.truncate(3);
    runtime.set_director_projection(projection.clone());
    assert_eq!(runtime.director_organization_cursor(), 2);
    let _ = runtime.select_tab_selection(TabSelection::Interrupted(second_id));
    assert_eq!(runtime.director_organization_cursor(), 0);
    runtime.set_director_projection(projection);
    assert_eq!(runtime.director_organization_cursor(), 0);
}

#[test]
fn organization_wheel_only_scrolls_the_focused_drawer_body_outside_launch_picker() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let target = Target::Root(workspace);
    let operation = OperationId::new();
    let _ = runtime.request_pane(target, operation, PaneKind::Agent);
    let _ = runtime.complete_pane(target, operation, scoped_terminal_ref(workspace, None));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let _ = runtime.apply_event(AppEvent::Resize {
        width: 100,
        height: 24,
    });
    runtime.set_director_projection(DirectorDrawerProjection {
        organization: vec![
            DirectorOrganizationRow {
                depth: 0,
                label: "row".into(),
                status: "running".into(),
            };
            3
        ],
        ..DirectorDrawerProjection::default()
    });
    runtime.scroll_director_organization(true, 1);
    let wheel = |column, row| {
        Key::Live(LiveTerminalAction::Wheel {
            up: false,
            column,
            row,
            notches: 1,
        })
    };
    for (column, row) in [(0, 15), (90, 0), (90, 23), (40, 15)] {
        assert_eq!(
            crate::presentation::handle_director_picker_input(&mut runtime, &wheel(column, row)),
            Some(Vec::new())
        );
        assert_eq!(runtime.director_organization_cursor(), 1);
    }
    let inside = wheel(90, 15);
    assert_eq!(
        crate::presentation::handle_director_picker_input(&mut runtime, &inside),
        Some(Vec::new())
    );
    assert_eq!(runtime.director_organization_cursor(), 2);
    runtime.scroll_director_organization(false, 1);
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(_)
    ));
    assert_eq!(
        crate::presentation::handle_director_picker_input(&mut runtime, &inside),
        Some(Vec::new())
    );
    assert_eq!(runtime.director_organization_cursor(), 1);
    let _ = runtime.handle_key(Key::Enter);
    assert!(runtime.state().director_launching().is_some());
    assert_eq!(
        crate::presentation::handle_director_picker_input(&mut runtime, &inside),
        Some(Vec::new())
    );
    assert_eq!(runtime.director_organization_cursor(), 1);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert_eq!(
        crate::presentation::handle_director_picker_input(&mut runtime, &inside),
        None
    );
    assert_eq!(runtime.director_organization_cursor(), 1);
}

#[test]
fn director_organization_is_scoped_to_the_selected_conversation_and_descendants() {
    use crate::usecase::application::controller::SessionRoleProjection;
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let ids = (0..5).map(|_| SessionId::new()).collect::<Vec<_>>();
    let mut workspace_state = state("demo");
    let template = workspace_state.sessions[0].clone();
    workspace_state.sessions = ["first", "second", "legacy", "grandchild", "child"]
        .into_iter()
        .map(|name| SessionRecord {
            name: name.into(),
            ..template.clone()
        })
        .collect();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), workspace_state, ids.clone());
    let role = |creator_continuation, parent_session_id| SessionRoleProjection {
        director_continuation: None,
        creator_continuation,
        parent_session_id,
        role_id: None,
        role_summary: None,
        agent_status: None,
    };
    view.set_session_roles(BTreeMap::from([
        (ids[0], role(Some(first), None)),
        (ids[1], role(Some(second), None)),
        (ids[2], role(None, None)),
        (ids[3], role(None, Some(ids[4]))),
        (ids[4], role(None, Some(ids[0]))),
    ]));
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let labels = |continuation| {
        director_organization(&ui, continuation)
            .into_iter()
            .map(|row| row.label)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels(Some(first)),
        [
            "♛ Director",
            "• Executor · first",
            "• Executor · child",
            "• Executor · grandchild"
        ]
    );
    assert_eq!(labels(Some(second)), ["♛ Director", "• Executor · second"]);
    assert_eq!(labels(None), ["♛ Director"]);
    assert_eq!(labels(Some(AgentContinuationRef::new())), ["♛ Director"]);

    // Restore interrupted conversations, then switch their stable selection.
    // Projection must follow the chosen conversation without launching either.
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, ids);
    let interrupted = [first, second]
        .into_iter()
        .map(|continuation| {
            let mut history = interrupted_history(workspace, None, true);
            history.continuation = continuation;
            history
        })
        .collect();
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: Some(first),
            interrupted,
        }]
    ));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let projected = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert_eq!(projected.organization.len(), 4);
    assert_eq!(projected.organization[0].status, "interrupted");
    let _ = runtime.select_tab_selection(TabSelection::Interrupted(second));
    let projected = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert_eq!(projected.organization.len(), 2);
    assert_eq!(projected.organization[1].label, "• Executor · second");
}

#[test]
fn background_director_keeps_its_selected_interrupted_conversation_detail() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let interrupted = interrupted_history(workspace, None, true);
    let continuation = interrupted.continuation;
    let detail = interrupted.safe_detail().to_owned();
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: Some(continuation),
            interrupted: vec![interrupted],
        }]
    ));
    let shell = scoped_terminal_ref(workspace, None);
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(Target::Root(workspace), operation, shell.clone());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let _ = runtime.handle_key(Key::Enter);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let before = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert_eq!(before.route, DirectorRoute::Console);
    assert_eq!(before.interrupted_detail.as_deref(), Some(detail.as_str()));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(runtime.focused_terminal(), Some(shell));
    assert_eq!(
        runtime.director_selection(),
        Some(TabSelection::Interrupted(continuation))
    );
    let background = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert!(!background.focused);
    assert_eq!(background.interrupted_detail, before.interrupted_detail);
    assert_eq!(background.organization, before.organization);
}

#[test]
fn background_director_does_not_steal_shell_tab_navigation() {
    let workspace = WorkspaceId::new();
    let root = Target::Root(workspace);
    let agent = scoped_terminal_ref(workspace, None);
    let shells = [
        scoped_terminal_ref(workspace, None),
        scoped_terminal_ref(workspace, None),
    ];
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    for (terminal, kind) in [
        (agent.clone(), PaneKind::Agent),
        (shells[0].clone(), PaneKind::Terminal),
        (shells[1].clone(), PaneKind::Terminal),
    ] {
        let operation = OperationId::new();
        let _ = runtime.request_pane(root, operation, kind);
        let _ = runtime.complete_pane(root, operation, terminal);
    }
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let shell_geometry = root_terminal_drawer::geometry(24, 100);
    focus_workspace_drawer_from_pointer(
        &mut runtime,
        &Key::Click {
            column: 2,
            row: u16::try_from(shell_geometry.top + 3).unwrap(),
        },
        24,
        100,
    );
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    let before = runtime.focused_terminal().unwrap();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending = std::collections::HashMap::new();
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        24,
        100,
        0,
        0
    ));
    assert_ne!(runtime.focused_terminal(), Some(before));
    assert!(shells.contains(&runtime.focused_terminal().unwrap()));
    assert_eq!(
        runtime.director_selection(),
        Some(TabSelection::Live(agent))
    );
    assert!(!crate::presentation::select_director_tab(
        &Key::Down,
        &mut ui,
        &mut runtime
    ));
    let director = director_drawer::geometry(24, 100);
    focus_workspace_drawer_from_pointer(
        &mut runtime,
        &Key::Click {
            column: u16::try_from(director.left + 2).unwrap(),
            row: u16::try_from(director.top + 4).unwrap(),
        },
        24,
        100,
    );
    assert!(!crate::presentation::select_root_terminal_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut runtime
    ));
}

#[test]
fn session_worktree_names_include_stale_directories_only() {
    let temp = tempdir().unwrap();
    let sessions = temp.path().join(".usagi/sessions");
    std::fs::create_dir_all(sessions.join("stale-session")).unwrap();
    std::fs::write(sessions.join("not-a-worktree"), "marker").unwrap();

    assert_eq!(
        FsSessionWorktreeScanPort.scan(temp.path()),
        vec!["stale-session"]
    );
}

#[test]
fn director_organization_projects_statuses_hierarchy_and_orphans() {
    use crate::usecase::application::controller::SessionRoleProjection;
    use usagi_core::domain::agent::AgentStatus;

    let conversation = AgentContinuationRef::new();
    let empty_ui = io_runtime(
        WorkspaceView::with_runtime_ids(ws("empty"), empty_state("empty"), Vec::new()),
        Box::new(UnavailableSessionCommandPort),
    );
    assert_eq!(
        director_organization(&empty_ui, None)[0].label,
        "♛ Director"
    );

    let director_child = SessionId::new();
    let manager_child = SessionId::new();
    let stopped_child = SessionId::new();
    let running_child = SessionId::new();
    let failed_child = SessionId::new();
    let orphan = SessionId::new();
    let ids = vec![
        director_child,
        manager_child,
        stopped_child,
        running_child,
        failed_child,
        orphan,
    ];
    let mut workspace_state = state("demo");
    let template = workspace_state.sessions[0].clone();
    workspace_state.sessions = [
        "manager", "worker", "stopped", "running", "failed", "orphan",
    ]
    .into_iter()
    .map(|name| SessionRecord {
        name: name.into(),
        root: PathBuf::from(format!("/tmp/demo/{name}")),
        ..template.clone()
    })
    .collect();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), workspace_state, ids.clone());
    let role = |parent_session_id, agent_status| SessionRoleProjection {
        director_continuation: None,
        creator_continuation: Some(conversation),
        role_id: None,
        role_summary: None,
        parent_session_id,
        agent_status,
    };
    let mut roles = BTreeMap::from([
        (director_child, role(None, Some(AgentStatus::Starting))),
        (
            manager_child,
            role(Some(director_child), Some(AgentStatus::Idle)),
        ),
        (
            stopped_child,
            role(Some(manager_child), Some(AgentStatus::Exited)),
        ),
        (running_child, role(None, Some(AgentStatus::Running))),
        (
            failed_child,
            role(Some(running_child), Some(AgentStatus::Failed)),
        ),
        // A corrupt self-cycle is emitted as a root-level orphan and must
        // not increase projection depth or loop forever.
        (orphan, role(Some(orphan), None)),
    ]);
    roles.get_mut(&director_child).unwrap().role_id =
        Some(usagi_core::domain::role::RoleId::new("manager").expect("valid company role"));
    view.set_session_roles(roles);
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));

    let rows = director_organization(&ui, Some(conversation));
    assert_eq!(
        rows.iter()
            .map(|row| (row.depth, row.label.as_str(), row.status.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (0, "♛ Director", "active"),
            (1, "◆ Manager · manager", "starting"),
            (2, "• Executor · worker", "waiting"),
            (3, "• Executor · stopped", "stopped"),
            (1, "• Executor · running", "running"),
            (2, "• Executor · failed", "failed"),
            (1, "• Executor · orphan", "ready"),
        ]
    );

    let mut runtime = WorkspaceRuntime::new(WorkspaceId::new(), ids);
    let _ = crate::presentation::sync_runtime_sessions(&mut runtime, &ui, &[]);
    let projected = crate::presentation::project_controller_sessions(&ui, runtime.state());
    assert_eq!(
        projected
            .iter()
            .map(|session| (session.label.as_str(), session.organization_depth))
            .collect::<Vec<_>>(),
        vec![
            ("manager", 0),
            ("worker", 1),
            ("stopped", 2),
            ("running", 0),
            ("failed", 1),
            ("orphan", 0),
        ]
    );
    assert_eq!(projected[1].parent_session_id, Some(director_child));
}

/// #554 acceptance. A closed create form has no reader for the hint, so the
/// frame budget must not contain a `read_dir` at all — this used to be ~62
/// directory scans per second plus one `stat` per entry, forever.
#[test]
fn a_closed_create_form_never_scans_the_sessions_directory() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut hint = SessionWorktreeHint::new(counting_scan(&scans));

    for tick in 0..600 {
        assert!(
            hint.names(false, std::path::Path::new("/tmp/demo"), at_tick(tick))
                .is_empty(),
            "a closed form must contribute no hint"
        );
    }

    assert_eq!(scans.load(Ordering::SeqCst), 0);
}

#[test]
#[allow(clippy::too_many_lines)] // One shell matrix fixes drawer mouse and pane ownership.
fn director_drawer_consumes_shell_pane_controls_without_background_mutation() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 181,
            replay: b"one\ntwo\nthree".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(runtime.state().director_drawer_open());
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    let tabs_before = runtime.active_pane().tabs().to_vec();
    let mut controls = LiveTerminalControls::default();
    let rows = vec!["one".to_owned(), "two".to_owned(), "three".to_owned()];
    let _ = controls.project(rows.clone(), 1);
    controls.scroll_up();
    let scroll_before = controls.project(rows.clone(), 1).scroll;
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let drawer = crate::presentation::director_drawer::geometry(20, 80);
    let new_click = Key::Click {
        column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
        row: u16::try_from(drawer.top + 2).unwrap(),
    };
    assert!(crate::presentation::is_director_new_click(
        &new_click, &runtime, 20, 80
    ));
    let new_pointer = Key::Pointer(PointerEvent {
        kind: PointerKind::Down,
        column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
        row: u16::try_from(drawer.top + 2).unwrap(),
    });
    assert!(crate::presentation::is_director_new_click(
        &new_pointer,
        &runtime,
        20,
        80
    ));
    assert!(!intercept_live_terminal_control(
        &new_pointer,
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        3,
        scroll_before,
    ));
    let managed_before = runtime
        .panes()
        .pane(Target::Session(session))
        .unwrap()
        .tabs()
        .to_vec();
    let launch_effects =
        crate::presentation::open_director_from_new_button(&mut runtime, &new_pointer, 20, 80)
            .expect("the Director New button owns its pointer press");
    assert!(launch_effects.is_empty());
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(_)
    ));
    assert_eq!(runtime.panes().active(), Some(Target::Root(workspace)));
    assert_eq!(
        runtime
            .panes()
            .pane(Target::Session(session))
            .unwrap()
            .tabs(),
        managed_before.as_slice()
    );
    assert!(!crate::presentation::is_director_new_click(
        &new_pointer,
        &runtime,
        20,
        80
    ));
    assert!(intercept_live_terminal_control(
        &new_pointer,
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        3,
        scroll_before,
    ));
    assert_eq!(
        launch_effects
            .iter()
            .filter(|effect| matches!(effect, Effect::LaunchAgent { .. }))
            .count(),
        0
    );
    assert!(intercept_live_terminal_control(
        &Key::Pointer(PointerEvent {
            kind: PointerKind::Up,
            column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
            row: u16::try_from(drawer.top + 2).unwrap(),
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        3,
        scroll_before,
    ));
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(_)
    ));
    assert_eq!(
        runtime
            .panes()
            .pane(Target::Session(session))
            .unwrap()
            .tabs(),
        managed_before.as_slice()
    );

    for key in [
        Key::Live(LiveTerminalAction::NextTab),
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
            3,
            scroll_before,
        ));
    }
    assert_eq!(controls.project(rows, 1).scroll, scroll_before);
    assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
    assert!(term.copied.is_empty());
    assert!(browser.opened.is_empty());
}

#[test]
fn drawer_header_buttons_remain_active_while_the_director_picker_owns_input() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
        DefaultModel::Claude,
    );
    assert!(
        runtime
            .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
            .is_empty()
    );
    assert!(runtime.state().director_drawer_open());
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(_)
    ));

    let home = HomeProjection::from_state(runtime.state(), "demo", &[]);
    let director_click = Key::Click { column: 99, row: 0 };
    assert_eq!(
        workspace_drawer_header_key(
            &Key::Pointer(PointerEvent {
                kind: PointerKind::Down,
                column: 99,
                row: 0,
            }),
            100,
            &home,
        ),
        Some(AppKey::ToggleDirectorDrawer)
    );
    assert_eq!(workspace_drawer_header_key(&Key::Other, 100, &home), None);
    let background_click = Key::Click { column: 0, row: 1 };
    assert_eq!(
        workspace_drawer_header_key(&background_click, 100, &home),
        None
    );
    assert_eq!(
        apply_drawer_header_while_director_open(&mut runtime, &background_click, 100, &home),
        None
    );

    let shell_column = (0..100)
        .find(|column| {
            crate::presentation::home_header_action_at(100, &home, *column, 0)
                == Some(crate::presentation::HomeHeaderAction::RootTerminal)
        })
        .expect("the wide Home header exposes Shell");
    let shell_click = Key::Click {
        column: shell_column,
        row: 0,
    };
    let effects = apply_drawer_header_while_director_open(&mut runtime, &shell_click, 100, &home)
        .expect("the Director priority seam must retain the Shell button");
    assert!(matches!(
        effects.as_slice(),
        [Effect::OpenTerminal {
            target: Target::Root(actual),
            arguments,
            ..
        }] if *actual == workspace && arguments == "open"
    ));
    assert!(runtime.state().director_drawer_open());
    assert!(runtime.state().root_terminal_drawer_open());
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );

    // The picker is intentionally exclusive, so the frame loop must resolve
    // the persistent header through this dedicated route first.
    let _ = runtime.apply_event(AppEvent::WorkspaceDrawerFocused(
        WorkspaceDrawerFocus::Director,
    ));
    assert_eq!(
        apply_drawer_header_while_director_open(&mut runtime, &director_click, 100, &home),
        Some(Vec::new())
    );
    assert!(!runtime.state().director_drawer_open());

    // Once closed, the same visible button belongs to the ordinary Home
    // route instead of this close-only priority seam.
    let closed = HomeProjection::from_state(runtime.state(), "demo", &[]);
    assert_eq!(
        apply_drawer_header_while_director_open(&mut runtime, &director_click, 100, &closed),
        None
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers every admitted and refused drawer slot.
fn director_projection_and_tab_cycle_cover_every_agent_only_slot() {
    let workspace = WorkspaceId::new();
    let live = scoped_terminal_ref(workspace, None);
    let live_continuation = AgentContinuationRef::new();
    let interrupted = interrupted_history(workspace, None, true);
    let interrupted_continuation = interrupted.continuation;
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation: live_continuation,
        terminal: live.clone(),
        select: true,
    });
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation: interrupted_continuation,
        terminal: interrupted.last_terminal.clone(),
        select: false,
    });
    let durable = Arc::new(Mutex::new(intent));
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
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    let _ = runtime.handle_key(Key::Escape);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: vec![LivePane {
                terminal: live.clone(),
                kind: PaneKind::Agent,
            }],
            selected: Some(live.clone()),
            selected_interrupted: None,
            interrupted: vec![interrupted],
        }],
    ));

    // Closed drawers deliberately project nothing.
    assert_eq!(
        crate::presentation::director_drawer_projection(&ui, &runtime, None),
        crate::presentation::DirectorDrawerProjection::default()
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let terminal_view = TerminalViewProjection {
        rows: vec!["retained output".to_owned()],
        row_offset: 0,
        total_rows: 1,
        scroll: 0,
        feedback: Some("reconnecting".to_owned()),
    };
    let projected =
        crate::presentation::director_drawer_projection(&ui, &runtime, Some(&terminal_view));
    assert_eq!(projected.conversations.len(), 2);
    assert!(projected.conversations[0].selected);
    assert!(!projected.organization.is_empty());
    assert_eq!(projected.organization[0].label, "♛ Director");
    assert_eq!(projected.terminal_view, Some(terminal_view));
    assert_eq!(projected.interrupted_detail, None);

    // A live projection without control feedback still crosses the seam as
    // a projection; the adapter never converts its rows into drawer lines.
    let quiet_terminal_view = TerminalViewProjection {
        rows: vec!["quiet retained output".to_owned()],
        row_offset: 0,
        total_rows: 1,
        scroll: 0,
        feedback: None,
    };
    assert_eq!(
        crate::presentation::director_drawer_projection(&ui, &runtime, Some(&quiet_terminal_view))
            .terminal_view,
        Some(quiet_terminal_view)
    );

    // Closing the selected live Agent is a no-op. The daemon-owned tab and
    // selection remain intact until the CLI exits.
    let mut pending_targets = std::collections::HashMap::new();
    crate::presentation::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending_targets);
    let interrupted_projection =
        crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert!(interrupted_projection.conversations[0].selected);
    assert_eq!(interrupted_projection.interrupted_detail, None);
    assert_eq!(interrupted_projection.terminal_view, None);
    runtime.fail_tab_resume_for(
        Target::Root(workspace),
        interrupted_continuation,
        None,
        "safe retry feedback".to_owned(),
    );
    let failed_projection = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert_eq!(failed_projection.interrupted_detail, None);
    assert_eq!(
        failed_projection.feedback.as_deref(),
        Some("safe retry feedback")
    );
    let live_failure_view = TerminalViewProjection {
        rows: vec!["older live Director".to_owned()],
        row_offset: 0,
        total_rows: 1,
        scroll: 0,
        feedback: None,
    };
    let live_failure_projection =
        crate::presentation::director_drawer_projection(&ui, &runtime, Some(&live_failure_view));
    assert_eq!(
        live_failure_projection.feedback.as_deref(),
        Some("safe retry feedback"),
        "management routes keep launch failure feedback beside an older live Director"
    );
    assert!(crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    let selected_interrupted = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert!(selected_interrupted.conversations[1].selected);
    assert!(selected_interrupted.interrupted_detail.is_some());
    assert!(crate::presentation::select_director_tab_and_activate(
        &Key::Live(LiveTerminalAction::PreviousTab),
        &mut ui,
        &mut runtime,
        &mut pending_targets,
    ));
    assert_eq!(runtime.focused_terminal(), Some(live));
    assert!(crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    let pending = OperationId::new();
    let _ = runtime.request_pane(Target::Root(workspace), pending, PaneKind::Agent);
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Select(
            crate::usecase::application::pane::PaneSelection::Tab(TabSelection::Pending(pending)),
        ),
    );
    let pending_projection = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert!(
        pending_projection
            .conversations
            .iter()
            .any(|conversation| conversation.label == "Agent (starting)" && conversation.selected)
    );
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Select(
            crate::usecase::application::pane::PaneSelection::Tab(TabSelection::Interrupted(
                interrupted_continuation,
            )),
        ),
    );
    assert!(crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    assert_eq!(
        runtime.active_pane().selected(),
        &crate::usecase::application::pane::PaneSelection::Tab(TabSelection::Pending(pending))
    );

    // Bypass runtime admission to prove the projection independently drops
    // every generic/diff shape if an impossible state reaches it.
    let generic = scoped_terminal_ref(workspace, None);
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Restore(LivePane {
            terminal: generic,
            kind: PaneKind::Terminal,
        }),
    );
    let unobserved_agent = scoped_terminal_ref(workspace, None);
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Restore(LivePane {
            terminal: unobserved_agent.clone(),
            kind: PaneKind::Agent,
        }),
    );
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Select(
            crate::usecase::application::pane::PaneSelection::Tab(TabSelection::Live(
                unobserved_agent,
            )),
        ),
    );
    let generic_pending = OperationId::new();
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Request {
            operation: generic_pending,
            target: Target::Root(workspace),
            kind: PaneKind::Terminal,
        },
    );
    let diff = OperationId::new();
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Request {
            operation: diff,
            target: Target::Root(workspace),
            kind: PaneKind::Diff,
        },
    );
    runtime.inject_pane_event_for_test(
        Target::Root(workspace),
        crate::usecase::application::pane::PaneEvent::Resolved { operation: diff },
    );
    let filtered = crate::presentation::director_drawer_projection(&ui, &runtime, None);
    assert_eq!(filtered.conversations.len(), 4);
    assert!(
        filtered
            .conversations
            .iter()
            .any(|conversation| conversation.label == "Agent" && conversation.selected)
    );

    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    for action in [
        LiveTerminalAction::MoveTabNext,
        LiveTerminalAction::MoveTabPrevious,
    ] {
        assert!(intercept_live_terminal_control(
            &Key::Live(action),
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
    }
    assert!(!crate::presentation::select_director_tab(
        &Key::Char('x'),
        &mut ui,
        &mut runtime,
    ));
    assert!(!crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::OpenPullRequests),
        &mut ui,
        &mut runtime,
    ));
    assert!(crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    assert!(crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::PreviousTab),
        &mut ui,
        &mut runtime,
    ));
    assert!(crate::presentation::select_director_tab(
        &Key::Down,
        &mut ui,
        &mut runtime,
    ));
    assert!(crate::presentation::select_director_tab(
        &Key::Up,
        &mut ui,
        &mut runtime,
    ));
    assert!(mutations.lock().unwrap().iter().any(|mutation| matches!(
        mutation,
        AgentTabIntentMutation::Select {
            session_id: None,
            ..
        }
    )));
}

#[test]
fn director_projection_covers_picker_empty_and_launching_states() {
    let workspace = WorkspaceId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::Agy]),
        DefaultModel::Agy,
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
    assert_eq!(
        crate::presentation::director_drawer_projection(&ui, &runtime, None).new,
        crate::presentation::DirectorNewProjection::Choosing {
            candidates: vec!["claude".to_owned(), "agy".to_owned()],
            selected: 1,
        }
    );

    let _ = runtime.handle_key(Key::Escape);
    runtime.set_agent_models(AvailableModels::default(), DefaultModel::OpenAi);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
    assert_eq!(
        crate::presentation::director_drawer_projection(&ui, &runtime, None).new,
        crate::presentation::DirectorNewProjection::Empty
    );

    let _ = runtime.handle_key(Key::Escape);
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
    let effects = runtime.handle_key(Key::Enter);
    assert!(matches!(effects.as_slice(), [Effect::LaunchAgent { .. }]));
    assert_eq!(
        crate::presentation::director_drawer_projection(&ui, &runtime, None).new,
        crate::presentation::DirectorNewProjection::Launching
    );
}

#[test]
fn director_tab_cycle_fails_closed_when_intent_cannot_commit() {
    let workspace = WorkspaceId::new();
    let first = scoped_terminal_ref(workspace, None);
    let second = scoped_terminal_ref(workspace, None);
    let first_continuation = AgentContinuationRef::new();
    let second_continuation = AgentContinuationRef::new();
    let mut intent = AgentTabIntent::empty(workspace);
    for (continuation, terminal, select) in [
        (first_continuation, first.clone(), true),
        (second_continuation, second.clone(), false),
    ] {
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation,
            terminal,
            select,
        });
    }
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::new(),
            Box::new(FailingIntentPort {
                state: Arc::new(Mutex::new(intent)),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: vec![
                LivePane {
                    terminal: first.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second,
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    assert!(!crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(crate::presentation::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    assert_eq!(runtime.focused_terminal(), Some(first));
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );
}

#[test]
fn director_pointer_uses_the_drawer_viewport() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort)).with_agent_context(
        workspace,
        Vec::new(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 919,
            replay: b"drawer output".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![crate::presentation::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: vec![LivePane {
                terminal: terminal.clone(),
                kind: PaneKind::Agent,
            }],
            selected: Some(terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let _ = runtime.handle_key(Key::Enter);
    ui.start_terminal_session(
        terminal,
        foreground_terminal_geometry(
            20,
            80,
            true,
            false,
            false,
            Some(WorkspaceDrawerFocus::Director),
        ),
    );
    let mut controls = LiveTerminalControls::default();
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
        1,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 26,
            row: 5,
        },
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // The production route matrix intentionally names every input vocabulary variant.
fn production_route_makes_director_picker_the_exclusive_foreground_owner() {
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
                terminal: root_agent,
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
    let open_picker = Key::Live(LiveTerminalAction::DirectorNew);
    assert!(runtime.handle_key(open_picker).is_empty());

    let inert_inputs = vec![
        Key::Live(LiveTerminalAction::Switch),
        Key::Live(LiveTerminalAction::OpenCloseupModal),
        Key::Live(LiveTerminalAction::NextTab),
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Live(LiveTerminalAction::MoveTabNext),
        Key::Live(LiveTerminalAction::MoveTabPrevious),
        Key::Live(LiveTerminalAction::Agent),
        Key::Live(LiveTerminalAction::DirectorNew),
        Key::Live(LiveTerminalAction::CloseTab),
        Key::Live(LiveTerminalAction::ResumeTab),
        Key::Live(LiveTerminalAction::QuitConfirmation),
        Key::Live(LiveTerminalAction::ScrollUp),
        Key::Live(LiveTerminalAction::ScrollDown),
        Key::Passthrough(b"raw".to_vec()),
        Key::Paste("paste".to_owned()),
        Key::TerminalCopy {
            fallback: b"copy".to_vec(),
        },
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 40,
            row: 5,
        }),
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
        Key::Backspace,
        Key::Tab,
        Key::CtrlQ,
        Key::CtrlD,
        Key::Char('x'),
        Key::Click { column: 1, row: 1 },
    ];
    let pane_before = runtime.active_pane().clone();
    for key in &inert_inputs {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                key,
            ),
            WorkspaceInputRoute::Drawer(Vec::new()),
            "picker did not own {key:?}"
        );
    }
    assert_eq!(runtime.active_pane(), &pane_before);
    assert!(inputs.lock().unwrap().is_empty());

    // Runtime-only wakeups cross the owner gate. Backend events are drained
    // before this seam by the production loop; Resize/Other are its terminal
    // wake vocabulary and likewise stay downstream.
    for key in [Key::Resize, Key::Other] {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &key,
            ),
            WorkspaceInputRoute::Unhandled,
        );
    }

    // Ctrl-C cancels Choosing and returns to Organization. Enter explicitly
    // opens the selected Director Console; only that surface forwards Agent
    // PTY input.
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Quit,
        ),
        WorkspaceInputRoute::Drawer(Vec::new()),
    );
    assert_eq!(runtime.state().director_new(), DirectorNew::Idle);
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Enter,
        ),
        WorkspaceInputRoute::Drawer(Vec::new()),
    );
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Char('z'),
        ),
        WorkspaceInputRoute::Forwarded,
    );
    assert_eq!(
        inputs
            .lock()
            .unwrap()
            .last()
            .map(|(_, bytes)| bytes.as_slice()),
        Some(b"z".as_slice())
    );
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Enter,
        ),
        WorkspaceInputRoute::Forwarded,
    );
    assert_eq!(
        inputs
            .lock()
            .unwrap()
            .last()
            .map(|(_, bytes)| bytes.as_slice()),
        Some(b"\r".as_slice())
    );
    inputs.lock().unwrap().clear();

    // Empty has the same exclusive ownership even though it has no
    // selectable row and Enter cannot launch anything.
    runtime.set_agent_models(AvailableModels::default(), DefaultModel::Claude);
    assert!(
        runtime
            .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
            .is_empty()
    );
    assert_eq!(runtime.state().director_new(), DirectorNew::Empty);
    for key in inert_inputs
        .iter()
        .chain([Key::Up, Key::Down, Key::Enter].iter())
    {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                key,
            ),
            WorkspaceInputRoute::Drawer(Vec::new()),
            "empty projection did not own {key:?}"
        );
    }
    assert!(inputs.lock().unwrap().is_empty());
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Drawer(Vec::new()),
    );
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
        DefaultModel::Claude,
    );
    assert!(
        runtime
            .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
            .is_empty()
    );

    // Reserved picker operations remain live. Navigation stays local,
    // Enter emits exactly one launch, and launch-pending remains exclusive.
    for key in [Key::Down, Key::Up] {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &key,
            ),
            WorkspaceInputRoute::Drawer(Vec::new()),
        );
    }
    let WorkspaceInputRoute::Drawer(launch) = route_workspace_input_before_reducer(
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &Key::Enter,
    ) else {
        panic!("picker Enter was not owned");
    };
    assert!(matches!(
        launch.as_slice(),
        [Effect::LaunchAgent { session: None, .. }]
    ));
    assert!(runtime.state().director_launching().is_some());
    for key in &inert_inputs {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                key,
            ),
            WorkspaceInputRoute::Drawer(Vec::new()),
            "launching projection did not own {key:?}"
        );
    }
    assert!(inputs.lock().unwrap().is_empty());

    // The Director chord still closes the foreground owner. The immediately
    // following ordinary input uses the restored downstream PTY route.
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::Director),
        ),
        WorkspaceInputRoute::Drawer(Vec::new()),
    );
    assert!(!runtime.state().director_drawer_open());
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Char('z'),
        ),
        WorkspaceInputRoute::Unhandled,
    );
}

#[test]
fn clicking_the_exposed_shell_focuses_it_and_keeps_copy_available_under_director() {
    let workspace = WorkspaceId::new();
    let root = Target::Root(workspace);
    let agent = scoped_terminal_ref(workspace, None);
    let shell = scoped_terminal_ref(workspace, None);
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    assert_eq!(
        focus_workspace_drawer_from_pointer(
            &mut runtime,
            &Key::Click { column: 0, row: 0 },
            24,
            100,
        ),
        None
    );
    for (terminal, kind) in [
        (agent, PaneKind::Agent),
        (shell.clone(), PaneKind::Terminal),
    ] {
        let operation = OperationId::new();
        let _ = runtime.request_pane(root, operation, kind);
        let _ = runtime.complete_pane(root, operation, terminal);
    }
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );

    let root_geometry = root_terminal_drawer::geometry(24, 100);
    assert_eq!(
        focus_workspace_drawer_from_pointer(
            &mut runtime,
            &Key::Click {
                column: 2,
                row: u16::try_from(root_geometry.top + 3).unwrap(),
            },
            24,
            100,
        ),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert_eq!(runtime.focused_terminal(), Some(shell.clone()));

    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&shell));
    let mut selection = TerminalSelection::begin(
        vec!["shell output".to_owned()],
        TerminalPoint { row: 0, column: 0 },
    );
    selection.extend(TerminalPoint { row: 0, column: 4 });
    controls.begin_selection(selection);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = io_runtime(view, Box::new(UnavailableSessionCommandPort));
    let mut term = FakeTerminal::default();
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: Vec::new(),
        },
    ));
    assert_eq!(term.copied, ["shell".to_owned()]);

    let director = director_drawer::geometry(24, 100);
    assert_eq!(
        focus_workspace_drawer_from_pointer(
            &mut runtime,
            &Key::Click {
                column: u16::try_from(director.left + 2).unwrap(),
                row: u16::try_from(director.top + 4).unwrap(),
            },
            24,
            100,
        ),
        Some(WorkspaceDrawerFocus::Director)
    );
    assert_eq!(
        focus_workspace_drawer_from_pointer(
            &mut runtime,
            &Key::Pointer(PointerEvent {
                kind: PointerKind::Down,
                column: u16::try_from(director.left + 3).unwrap(),
                row: u16::try_from(director.top + 5).unwrap(),
            }),
            24,
            100,
        ),
        Some(WorkspaceDrawerFocus::Director)
    );
}

#[test]
fn new_form_directory_completion_uses_the_loader_and_tolerates_io_failure() {
    let keys = [Key::Char('e'), Key::Right, Key::Down]
        .into_iter()
        .chain("/tmp/al".chars().map(Key::Char))
        .chain([Key::Tab, Key::Quit])
        .collect::<Vec<_>>();

    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        directory_entries: vec!["alpine".to_owned(), "alpha".to_owned()],
        ..FakeLoader::default()
    };
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.directory_requests, [PathBuf::from("/tmp")]);
    assert!(term.frames.iter().any(|frame| {
        crate::presentation::widgets::strip_ansi(&frame.join("\n")).contains("/tmp/alpha/")
    }));

    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        directory_error: Some(io::ErrorKind::PermissionDenied),
        ..FakeLoader::default()
    };
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.directory_requests, [PathBuf::from("/tmp")]);
    assert!(term.frames.iter().any(|frame| {
        crate::presentation::widgets::strip_ansi(&frame.join("\n")).contains("/tmp/al")
    }));
}

#[test]
fn project_add_accepts_and_opens_an_unregistered_directory_path() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Tab,
        Key::Paste("/tmp/external".to_owned()),
        Key::Enter,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    term.size = Some((24, 80));
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha")],
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
        vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/external")]
    );
    assert!(term.frames.iter().any(|frame| {
        let frame = frame.join("\n");
        frame.contains("Directory") && frame.contains("Tab registered")
    }));
}

#[test]
fn direct_welcome_recent_and_open_entries_share_the_director_drawer_shell() {
    let mut direct = FakeTerminal::with_keys(&[
        Key::Live(LiveTerminalAction::Director),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut direct_factory = FixedBackendFactory {
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
            run_workspace_controller_with_backend(
                &mut direct,
                snapshot("direct"),
                &mut direct_factory,
            )
            .unwrap(),
            Exit::Quit
        );
    assert!(has_director_drawer(&direct.frames));

    let mut recent_term = FakeTerminal::with_keys(&[
        Key::Char('r'),
        Key::Live(LiveTerminalAction::Director),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    run(
        &mut recent_term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(has_director_drawer(&recent_term.frames));

    let mut open_term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::Director),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    run(
        &mut open_term,
        vec![ws("open")],
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(has_director_drawer(&open_term.frames));
}

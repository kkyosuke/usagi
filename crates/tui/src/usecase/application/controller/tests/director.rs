//! director の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn director_drawer_toggle_preserves_background_state_and_owns_input() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 100,
            height: 30,
        },
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.active(), Some(second));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    // A tab-owning Closeup has no launcher modal, leaving the drawer entry
    // available without changing the active managed-session surface.
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    assert_eq!(state.overlay(), None);
    let background = (
        state.route(),
        state.overlay(),
        state.selected(),
        state.active(),
        state.size(),
        state.has_live_pane(),
        state.has_pane_tab,
        state.closeup_action_forced,
    );

    assert!(update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer)).is_empty());
    assert!(state.director_drawer_open());
    for event in [
        AppEvent::Key(AppKey::Up),
        AppEvent::Key(AppKey::CtrlA),
        AppEvent::Key(AppKey::OpenOverview),
        AppEvent::Key(AppKey::CtrlQ),
        AppEvent::Key(AppKey::Char('x')),
        AppEvent::Pointer {
            column: 1,
            row: 2,
            at: std::time::Duration::from_millis(1),
        },
    ] {
        assert!(update(&mut state, event).is_empty());
        assert!(state.director_drawer_open());
        assert_eq!(
            (
                state.route(),
                state.overlay(),
                state.selected(),
                state.active(),
                state.size(),
                state.has_live_pane(),
                state.has_pane_tab,
                state.closeup_action_forced,
            ),
            background
        );
    }

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert!(!state.director_drawer_open());
    assert_eq!(
        (
            state.route(),
            state.overlay(),
            state.selected(),
            state.active(),
            state.size(),
            state.has_live_pane(),
            state.has_pane_tab,
            state.closeup_action_forced,
        ),
        background
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(!state.director_drawer_open());
}

#[test]
fn director_routes_preserve_their_hierarchy_across_close_and_reopen() {
    let mut state = AppState::home(WorkspaceId::new(), Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert_eq!(state.director_route(), DirectorRoute::Organization);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorConsole));
    assert_eq!(state.director_route(), DirectorRoute::Console);
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(!state.director_drawer_open());
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert_eq!(state.director_route(), DirectorRoute::Console);
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.director_route(), DirectorRoute::Organization);
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert!(!state.director_drawer_open());
}

#[test]
fn director_route_commands_are_guarded_and_restore_the_organization() {
    let mut state = AppState::home(WorkspaceId::new(), Vec::new());
    state.director_new = DirectorNew::Empty;
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorOrganization
    ));
    assert_eq!(state.director_new(), DirectorNew::Idle);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorConsole
    ));
    state.director_launching = Some(OperationId::new());
    assert!(update_director_route_key(&mut state, &AppKey::DirectorBack));
    assert_eq!(state.director_route(), DirectorRoute::Console);
    state.director_launching = None;
    state.director_new = DirectorNew::Empty;
    director_back(&mut state);
    assert_eq!(state.director_new(), DirectorNew::Idle);
    assert_eq!(state.director_route(), DirectorRoute::Console);
    director_back(&mut state);
    assert_eq!(state.director_route(), DirectorRoute::Organization);
}

#[test]
fn director_new_picker_has_deterministic_candidates_and_cancel() {
    let workspace = WorkspaceId::new();
    let mut state = AppState::home(workspace, Vec::new());
    state.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::Agy]),
        DefaultModel::OpenAi,
    );
    let background = (state.selected(), state.active(), state.route());
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));

    // A missing configured default highlights the first installed candidate
    // in vocabulary order without confirming it.
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew)).is_empty());
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::Claude)
    );
    assert_eq!(
        (state.selected(), state.active(), state.route()),
        background
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::Agy)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::Claude)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::Agy)
    );

    // Escape cancels only the chooser; the drawer and every background
    // selection remain unchanged.
    assert!(update(&mut state, AppEvent::Key(AppKey::Escape)).is_empty());
    assert_eq!(state.director_new(), DirectorNew::Idle);
    assert!(state.director_drawer_open());
    assert_eq!(
        (state.selected(), state.active(), state.route()),
        background
    );
}

#[test]
fn director_picker_launches_a_root_agent() {
    let workspace = WorkspaceId::new();
    let mut state = sized_home(workspace, Vec::new(), 100, 30);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    let [
        Effect::LaunchAgent {
            session: None,
            operation_id,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("launch must emit one root Agent effect");
    };
    let _ = update(
        &mut state,
        AppEvent::DirectorLaunchFinished {
            operation: *operation_id,
            succeeded: false,
        },
    );
    assert_eq!(state.director_route(), DirectorRoute::Organization);
}

#[test]
fn empty_director_closes_and_returns_focus_to_an_open_workspace_terminal() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );

    assert!(update(&mut state, AppEvent::DirectorDrawerEmptied).is_empty());
    assert!(!state.director_drawer_open());
    assert!(state.root_terminal_drawer_open());
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
}

#[test]
fn shell_and_director_toggle_independently_from_either_focused_drawer() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    // Director first: the Shell opens and then closes from Director's side.
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    let effects = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    assert!(matches!(
        effects.as_slice(),
        [Effect::OpenTerminal { arguments, .. }] if arguments == "open"
    ));
    assert!(state.root_terminal_drawer_open());
    // Refocus Director while the Shell stays open, as a click on it would.
    let _ = update(
        &mut state,
        AppEvent::WorkspaceDrawerFocused(WorkspaceDrawerFocus::Director),
    );
    assert!(
        update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer)).is_empty(),
        "closing an open Shell from Director must not reopen the root terminal"
    );
    assert!(!state.root_terminal_drawer_open());
    assert!(state.director_drawer_open());
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );

    // Shell focused with Director open: the Director toggle closes Director
    // instead of only moving focus onto it.
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert!(update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer)).is_empty());
    assert!(!state.director_drawer_open());
    assert!(state.root_terminal_drawer_open());
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );

    // And Director reopens from the Shell, taking focus.
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(state.director_drawer_open());
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
}

#[test]
fn director_frontmost_transition_table_keeps_modal_and_background_ownership_unique() {
    struct Case {
        name: &'static str,
        modal: bool,
        events: Vec<AppKey>,
        drawer_open: bool,
        picker_open: bool,
        launches: usize,
    }
    let workspace = WorkspaceId::new();
    let cases = [
        Case {
            name: "modal blocks drawer entry",
            modal: true,
            events: vec![AppKey::ToggleDirectorDrawer],
            drawer_open: false,
            picker_open: false,
            launches: 0,
        },
        Case {
            name: "toggle closes drawer",
            modal: false,
            events: vec![AppKey::ToggleDirectorDrawer, AppKey::ToggleDirectorDrawer],
            drawer_open: false,
            picker_open: false,
            launches: 0,
        },
        Case {
            name: "picker escape returns to drawer",
            modal: false,
            events: vec![AppKey::OpenDirectorNew, AppKey::Escape],
            drawer_open: true,
            picker_open: false,
            launches: 0,
        },
        Case {
            name: "picker confirmation launches root only",
            modal: false,
            events: vec![AppKey::OpenDirectorNew, AppKey::Enter],
            drawer_open: true,
            picker_open: false,
            launches: 1,
        },
    ];
    for case in cases {
        let mut state = AppState::home(workspace, Vec::new());
        state.set_agent_models(
            AvailableModels::new([DefaultModel::OpenAi]),
            DefaultModel::OpenAi,
        );
        if case.modal {
            state.overlay = Some(Overlay::Overview);
        }
        let effects = case
            .events
            .into_iter()
            .flat_map(|key| update(&mut state, AppEvent::Key(key)))
            .collect::<Vec<_>>();
        assert_eq!(
            state.director_drawer_open(),
            case.drawer_open,
            "{}",
            case.name
        );
        assert_eq!(
            matches!(state.director_new(), DirectorNew::Choosing(_)),
            case.picker_open,
            "{}",
            case.name
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::LaunchAgent { session: None, .. }))
                .count(),
            case.launches,
            "{}",
            case.name
        );
        assert!(
            state.overlay().is_none() || !state.director_drawer_open(),
            "{}",
            case.name
        );
    }
}

#[test]
fn director_new_picker_covers_default_single_and_empty_availability() {
    let workspace = WorkspaceId::new();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));

    state.set_agent_models(AvailableModels::all(), DefaultModel::Agy);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::Agy)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));

    state.set_agent_models(
        AvailableModels::new([DefaultModel::OpenAi]),
        DefaultModel::Claude,
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::OpenAi)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::OpenAi)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));

    state.set_agent_models(AvailableModels::default(), DefaultModel::OpenAi);
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew)).is_empty());
    assert_eq!(state.director_new(), DirectorNew::Empty);
    assert_eq!(state.director_launching(), None);
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());

    // A composition-policy refresh racing an already-open chooser degrades
    // either movement direction to the same safe empty state.
    for key in [AppKey::Up, AppKey::Down] {
        state.director_new = DirectorNew::Choosing(DefaultModel::Claude);
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert_eq!(state.director_new(), DirectorNew::Empty);
    }
}

#[test]
fn director_picker_submits_one_explicit_root_launch_until_matching_finish() {
    let workspace = WorkspaceId::new();
    let mut state = AppState::home(workspace, Vec::new());
    state.set_agent_models(AvailableModels::new([DefaultModel::Agy]), DefaultModel::Agy);
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    let [
        Effect::LaunchAgent {
            workspace: launched_workspace,
            session,
            operation_id,
            profile,
        },
    ] = effects.as_slice()
    else {
        panic!("picker confirmation must emit exactly one launch: {effects:?}");
    };
    assert_eq!(*launched_workspace, workspace);
    assert_eq!(*session, None);
    assert_eq!(profile.as_ref().map(AgentProfileId::as_str), Some("agy"));
    assert_eq!(state.director_launching(), Some(*operation_id));

    // Reopening New, double Enter, and a stale completion cannot cross the
    // operation fence.
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    let _ = update(
        &mut state,
        AppEvent::DirectorLaunchFinished {
            operation: OperationId::new(),
            succeeded: true,
        },
    );
    assert_eq!(state.director_launching(), Some(*operation_id));
    let _ = update(
        &mut state,
        AppEvent::DirectorLaunchFinished {
            operation: *operation_id,
            succeeded: true,
        },
    );
    assert_eq!(state.director_launching(), None);
}

#[test]
fn director_picker_enter_is_inert_while_the_terminal_hides_every_candidate() {
    // The drawer below the persistent Home header draws its first candidate
    // row at 9 terminal rows; 8 rows reach the footer without one, so no
    // highlight is on screen.
    assert_eq!(director_picker_capacity(9), 1);
    assert_eq!(director_picker_capacity(8), 0);
    // An unmeasured terminal falls back to the renderer's normalized size
    // rather than locking the picker out before the first resize.
    assert_eq!(
        director_picker_capacity(0),
        NORMALIZED_TERMINAL_ROWS - DIRECTOR_PICKER_CHROME_ROWS
    );

    let workspace = WorkspaceId::new();
    for (height, launches) in [(8_u16, 0_usize), (9, 1)] {
        let mut state = AppState::home(workspace, Vec::new());
        state.set_agent_models(
            AvailableModels::new([DefaultModel::Claude]),
            DefaultModel::Claude,
        );
        let _ = update(&mut state, AppEvent::Resize { width: 80, height });
        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::LaunchAgent { .. }))
                .count(),
            launches,
            "height {height}"
        );
        assert_eq!(
            state.director_launching().is_some(),
            launches == 1,
            "height {height}"
        );
        // The refused Enter leaves the chooser open, so growing the terminal
        // confirms the same selection instead of restarting the flow.
        assert_eq!(
            matches!(state.director_new(), DirectorNew::Choosing(_)),
            launches == 0,
            "height {height}"
        );
    }

    // Growing the refused terminal releases the same selection.
    let mut state = AppState::home(workspace, Vec::new());
    state.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 80,
            height: 6,
        },
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 80,
            height: 24,
        },
    );
    assert_eq!(update(&mut state, AppEvent::Key(AppKey::Enter)).len(), 1);
}

#[test]
fn director_picker_maps_each_cli_fixture_to_one_explicit_profile() {
    let workspace = WorkspaceId::new();
    for (model, expected) in [
        (DefaultModel::Claude, "claude"),
        (DefaultModel::OpenAi, "codex"),
        (DefaultModel::Agy, "agy"),
    ] {
        let mut state = AppState::home(workspace, Vec::new());
        state.set_agent_models(AvailableModels::new([model]), model);
        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::LaunchAgent {
                session: None,
                profile: Some(profile),
                ..
            }] if profile.as_str() == expected
        ));
    }
}

#[test]
fn every_existing_modal_blocks_director_drawer_entry() {
    let (workspace, first, _) = ids();
    for overlay in [
        Overlay::Overview,
        Overlay::Daemon,
        Overlay::Closeup,
        Overlay::QuitConfirmation,
        Overlay::Notes,
        Overlay::Environment,
        Overlay::Roles,
        Overlay::CreateSession,
        Overlay::Decisions,
        Overlay::CleanupQueue,
        Overlay::RemoveSessions,
        Overlay::Prs,
        Overlay::Preview,
        Overlay::CreateSessionError,
        Overlay::TerminalLaunchError,
        Overlay::AgentLaunchError,
    ] {
        let mut state = AppState::home(workspace, vec![first]);
        state.overlay = Some(overlay);
        // Give each overlay the backing state its reducer requires, so a key
        // it does not recognise stays inert instead of closing a half-built
        // modal.
        match overlay {
            Overlay::CreateSession => {
                state.create_session = Some(CreateSessionForm::new(Vec::new()));
            }
            Overlay::Prs => {
                state.pr_overlay =
                    Some(PrOverlay::showing(Target::Session(first), Vec::new(), None));
            }
            Overlay::Preview => {
                state.preview_overlay =
                    Some(PreviewOverlay::loading(Target::Session(first), Vec::new()));
            }
            Overlay::Notes => {
                state.note_editor = Some(NoteEditor::loading(Target::Session(first)));
            }
            Overlay::Environment => {
                state.environment_editor = Some(EnvironmentEditor::loading(EnvScope::Workspace));
            }
            Overlay::Roles => {
                state.role_editor = Some(RoleEditor::loading(RoleEditorScope::Workspace));
            }
            Overlay::Decisions => {
                state.decision_overlay = Some(DecisionOverlayState {
                    selected: 0,
                    editor: None,
                });
            }
            Overlay::CleanupQueue => {
                state.cleanup_queue = Some(CleanupQueueState::new(Vec::new()));
            }
            Overlay::RemoveSessions => {
                state.remove_queue = Some(RemoveQueueState::new(Vec::new(), 0, false));
            }
            Overlay::Overview
            | Overlay::Daemon
            | Overlay::Closeup
            | Overlay::QuitConfirmation
            | Overlay::ForceRemoveConfirmation
            | Overlay::CreateSessionError
            | Overlay::TerminalLaunchError
            | Overlay::AgentLaunchError
            | Overlay::Garden => {}
        }
        for key in [AppKey::ToggleDirectorDrawer, AppKey::OpenDirectorNew] {
            assert!(update(&mut state, AppEvent::Key(key)).is_empty());
            assert_eq!(state.overlay(), Some(overlay));
            assert!(!state.director_drawer_open());
        }
    }
}

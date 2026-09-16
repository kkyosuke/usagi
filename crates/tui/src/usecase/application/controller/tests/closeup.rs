//! closeup の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn presentation_messages_are_bounded_and_terminal_safe() {
    let raw = format!("  failed\n\t\u{1b}[31m\u{202e}{}", "x".repeat(300));
    let safe = SafeMessage::new(&raw);
    let notice = Notice::new(raw);

    for message in [safe.as_str(), notice.message.as_str()] {
        assert!(presentation_text_is_safe(message));
        assert!(!message.starts_with(' '));
        assert!(message.chars().count() <= MAX_PRESENTATION_MESSAGE_CHARS);
        assert!(message.contains('\u{fffd}'));
        assert!(message.ends_with('…'));
    }

    assert_eq!(SafeMessage::new("  ready\n\t ").as_str(), "ready");
    assert_eq!(Notice::new("\n\t ").message, "");
}

#[test]
fn terminal_arguments_normalize_open_and_reject_untrusted_input() {
    assert_eq!(terminal_arguments("").unwrap(), "open");
    assert_eq!(terminal_arguments(" open ").unwrap(), "open");
    assert_eq!(terminal_arguments("new").unwrap(), "new");
    assert_eq!(
        terminal_arguments("--command sh").unwrap_err().message,
        "terminal accepts only `open` or `new`"
    );
}

#[test]
fn management_classifier_preserves_closeup_control_chords() {
    let ctrl_a = |code| {
        LiveInput::Key(crate::usecase::terminal_input::KeyEvent::new(
            code,
            crate::usecase::terminal_input::Modifiers {
                control: true,
                ..crate::usecase::terminal_input::Modifiers::default()
            },
            KeyEventKind::Press,
        ))
    };
    assert_eq!(
        classify_management_input(ctrl_a(KeyCode::Char('s'))),
        Some(AppKey::SaveRoles)
    );
    assert_eq!(
        classify_management_input(ctrl_a(KeyCode::Char('\u{1}'))),
        Some(AppKey::CtrlA)
    );
    assert_eq!(
        classify_management_input(ctrl_a(KeyCode::Char('a'))),
        Some(AppKey::CtrlA)
    );
    assert_eq!(
        classify_management_input(LiveInput::Key(
            crate::usecase::terminal_input::KeyEvent::new(
                KeyCode::Home,
                crate::usecase::terminal_input::Modifiers::default(),
                KeyEventKind::Press,
            )
        )),
        Some(AppKey::CtrlA)
    );
    for (code, expected) in [
        (KeyCode::PageUp, AppKey::PageUp),
        (KeyCode::PageDown, AppKey::PageDown),
    ] {
        assert_eq!(
            classify_management_input(LiveInput::Key(
                crate::usecase::terminal_input::KeyEvent::new(
                    code,
                    crate::usecase::terminal_input::Modifiers::default(),
                    KeyEventKind::Press,
                ),
            )),
            Some(expected),
        );
    }
    assert_eq!(
        classify_management_input(LiveInput::Key(
            crate::usecase::terminal_input::KeyEvent::new(
                KeyCode::Char('\u{f}'),
                crate::usecase::terminal_input::Modifiers::default(),
                KeyEventKind::Press,
            ),
        )),
        Some(AppKey::CtrlO)
    );
    for code in [KeyCode::Char('\u{f}'), KeyCode::Char('o')] {
        assert_eq!(classify_management_input(ctrl_a(code)), Some(AppKey::CtrlO));
    }
    for code in [KeyCode::Char('\u{e}'), KeyCode::Char('n')] {
        assert_eq!(classify_management_input(ctrl_a(code)), Some(AppKey::CtrlN));
    }
    for code in [KeyCode::Char('\u{10}'), KeyCode::Char('p')] {
        assert_eq!(classify_management_input(ctrl_a(code)), Some(AppKey::CtrlP));
    }
    for code in [KeyCode::Char('\u{18}'), KeyCode::Char('x')] {
        assert_eq!(classify_management_input(ctrl_a(code)), Some(AppKey::CtrlX));
    }
    assert_eq!(
        classify_management_input(LiveInput::Key(
            crate::usecase::terminal_input::KeyEvent::new(
                KeyCode::Char('X'),
                crate::usecase::terminal_input::Modifiers {
                    control: true,
                    shift: true,
                    ..crate::usecase::terminal_input::Modifiers::default()
                },
                KeyEventKind::Press,
            )
        )),
        Some(AppKey::CtrlX)
    );
}

#[test]
fn new_validation_rejects_terminal_and_direction_controls() {
    let clone_cases = [
        ("repository", NewValidationError::RepositoryInvalid),
        ("location", NewValidationError::LocationInvalid),
        ("directory", NewValidationError::DirectoryInvalid),
        ("branch", NewValidationError::BranchInvalid),
    ];
    for (field, expected) in clone_cases {
        let mut form = clone_form();
        match field {
            "repository" => form.repository = "https://example.com/unsafe\nrepo".to_owned(),
            "location" => form.location = "/work\u{7}/child".to_owned(),
            "directory" => form.directory = "app\u{202e}txt".to_owned(),
            "branch" => form.branch = "feature/\u{2066}name".to_owned(),
            _ => unreachable!(),
        }
        assert_eq!(validate_new_form(NewMode::Clone, &form), Err(expected));
    }

    let mut unsafe_path = existing_form();
    unsafe_path.path = "/work\r/existing".to_owned();
    assert_eq!(
        validate_new_form(NewMode::Existing, &unsafe_path),
        Err(NewValidationError::PathInvalid)
    );
    let mut unsafe_name = existing_form();
    unsafe_name.name.push('\u{202e}');
    assert_eq!(
        validate_new_form(NewMode::Existing, &unsafe_name),
        Err(NewValidationError::NameInvalid)
    );

    for (error, message) in [
        (
            NewValidationError::RepositoryInvalid,
            "repository URL must be a single safe line",
        ),
        (
            NewValidationError::LocationInvalid,
            "clone location must be a single safe line",
        ),
        (
            NewValidationError::BranchInvalid,
            "branch name must be a single safe line",
        ),
        (
            NewValidationError::PathInvalid,
            "directory path must be a single safe line",
        ),
        (
            NewValidationError::NameInvalid,
            "workspace name must be a single safe line",
        ),
    ] {
        assert_eq!(error.message(), message);
    }
}

#[test]
fn goal_composer_normalizes_paste_and_rejects_terminal_controls() {
    let workspace = WorkspaceId::new();
    let mut state = sized_home(workspace, Vec::new(), 100, 30);
    state.set_work_mode(WorkMode::GoalDriven);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));

    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::Paste(
            "first\r\nsecond\t\u{2028}\u{a0} \u{1b}[2J\u{7}third\u{202e} fourth".to_owned(),
        )),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('\u{9b}')));

    assert_eq!(state.director_goal(), "first second [2Jthird fourth");
    assert!(!state.director_goal().chars().any(char::is_control));
    assert!(!state.director_goal().chars().any(is_bidi_control));
}

#[test]
fn root_terminal_drawer_opens_root_shell_and_preserves_background_state() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    let background = (
        state.route(),
        state.selected(),
        state.active(),
        state.overlay(),
    );

    let effects = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    assert!(state.root_terminal_drawer_open());
    assert!(!state.root_terminal_full_height());
    assert!(!state.director_drawer_open());
    assert_eq!(
        effects.as_slice(),
        [Effect::OpenTerminal {
            target: Target::Root(workspace),
            operation_id: match &effects[0] {
                Effect::OpenTerminal { operation_id, .. } => *operation_id,
                _ => unreachable!(),
            },
            arguments: "open".to_owned(),
        }]
    );
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::ToggleRootTerminalFullHeight)
        )
        .is_empty()
    );
    assert!(state.root_terminal_full_height());
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::ToggleRootTerminalFullHeight)
        )
        .is_empty()
    );
    assert!(!state.root_terminal_full_height());
    for key in [AppKey::Up, AppKey::OpenOverview, AppKey::Escape] {
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert!(state.root_terminal_drawer_open());
        assert_eq!(
            (
                state.route(),
                state.selected(),
                state.active(),
                state.overlay()
            ),
            background
        );
    }

    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::ToggleRootTerminalFullHeight),
    );
    assert!(state.root_terminal_full_height());
    assert!(update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer)).is_empty());
    assert!(!state.root_terminal_full_height());
    assert!(state.root_terminal_drawer_open());
    assert!(state.director_drawer_open());
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
    assert_eq!(
        (
            state.route(),
            state.selected(),
            state.active(),
            state.overlay()
        ),
        background
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew)).is_empty());
    assert!(state.root_terminal_drawer_open());
    assert!(state.director_drawer_open());
    assert!(matches!(state.director_new(), DirectorNew::Choosing(_)));
}

#[test]
fn empty_root_terminal_drawer_closes_without_replaying_the_user_toggle() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    let effects = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    assert!(matches!(effects.as_slice(), [Effect::OpenTerminal { .. }]));
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert!(update(&mut state, AppEvent::RootTerminalDrawerEmptied).is_empty());
    assert!(!state.root_terminal_drawer_open());
    assert_eq!(state.workspace_drawer_focus(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
    assert!(update(&mut state, AppEvent::RootTerminalDrawerEmptied).is_empty());
    assert!(!state.root_terminal_drawer_open());
    assert!(state.director_drawer_open());
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
}

#[test]
fn switch_ctrl_c_is_ignored_while_closeup_preserves_existing_quit_behavior() {
    let (workspace, session, _) = ids();
    let mut idle = AppState::home(workspace, Vec::new());
    assert!(update(&mut idle, AppEvent::Key(AppKey::CtrlC)).is_empty());
    assert_eq!(idle.route(), Route::Home(HomeMode::Switch));
    assert_eq!(idle.overlay(), None);

    let mut live = AppState::home(workspace, vec![session]);
    let _ = update(&mut live, AppEvent::LivePaneAvailability(true));
    let _ = update(&mut live, AppEvent::Key(AppKey::Enter));
    assert_eq!(live.route(), Route::Home(HomeMode::Closeup));
    assert!(live.has_live_pane());
    assert!(update(&mut live, AppEvent::Key(AppKey::CtrlC)).is_empty());
    assert_eq!(live.overlay(), Some(Overlay::QuitConfirmation));

    // Confirmation is deliberately immune to repeated quit chords.
    for key in [AppKey::CtrlC, AppKey::CtrlQ] {
        assert!(update(&mut live, AppEvent::Key(key)).is_empty());
        assert_eq!(live.overlay(), Some(Overlay::QuitConfirmation));
    }
    assert_eq!(
        update(&mut live, AppEvent::Key(AppKey::Char('Y'))),
        vec![Effect::Detach]
    );
    assert_eq!(live.overlay(), None);
}

#[test]
fn switch_ctrl_c_never_detaches_after_leaving_a_live_pane() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
    assert!(state.ctrl_c_grace());
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
    assert!(state.ctrl_c_grace());

    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert!(!state.ctrl_c_grace());
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
}

#[test]
fn live_pane_availability_reacts_on_the_edge_not_the_level() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    assert!(state.has_live_pane());
    assert_eq!(state.overlay(), None);

    // A quit confirmation over the live pane survives a re-sampled, unchanged
    // live level (the runtime resamples on every event).
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlC));
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));

    // Leaving the pane arms the grace once; a repeated non-live level keeps it.
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('n')));
    let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
    assert!(state.ctrl_c_grace());
    assert_eq!(state.overlay(), None);
    let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
    assert!(state.ctrl_c_grace());
}

/// Escape and Ctrl-C close only the Closeup action modal and return input to
/// the underlying Closeup, while Ctrl-Q stays inert like every other overlay.
#[test]
fn closeup_action_modal_returns_to_closeup_on_escape_and_ctrl_c() {
    let (workspace, session, _) = ids();
    for exit_key in [AppKey::Escape, AppKey::CtrlC] {
        // Enter Closeup on a session with no live pane, then explicitly
        // open its action modal.
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
        assert_eq!(state.overlay(), None);
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.overlay(), Some(Overlay::Closeup));

        // Ctrl-Q keeps the modal, matching the other overlays' swallow.
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlQ)).is_empty());
        assert_eq!(state.overlay(), Some(Overlay::Closeup));

        // The exit key closes only the modal and lands on Closeup.
        assert!(update(&mut state, AppEvent::Key(exit_key.clone())).is_empty());
        assert_eq!(
            state.route(),
            Route::Home(HomeMode::Closeup),
            "{exit_key:?}"
        );
        assert_eq!(state.overlay(), None, "{exit_key:?}");
    }
}

#[test]
fn empty_closeup_uses_primary_shortcuts_and_enter_opens_actions() {
    let (workspace, session, _) = ids();
    let closeup = || {
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
        assert_eq!(state.overlay(), None);
        state
    };

    let mut agent = closeup();
    assert!(matches!(
        update(&mut agent, AppEvent::Key(AppKey::Char('a'))).as_slice(),
        [Effect::LaunchAgent {
            session: Some(actual),
            ..
        }] if *actual == session
    ));
    assert_eq!(agent.overlay(), None);

    let mut terminal = closeup();
    assert!(matches!(
        update(&mut terminal, AppEvent::Key(AppKey::Char('t'))).as_slice(),
        [Effect::OpenTerminal {
            target: Target::Session(actual),
            arguments,
            ..
        }] if *actual == session && arguments == "open"
    ));
    assert_eq!(terminal.overlay(), None);

    let mut actions = closeup();
    assert!(update(&mut actions, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(actions.overlay(), Some(Overlay::Closeup));
}

/// Even when the action modal is forced over a live pane, Escape and Ctrl-C
/// hand input back to that pane, and a trailing live resample does not
/// resurrect the overlay.
#[test]
fn closeup_forced_action_modal_returns_to_the_live_closeup() {
    let (workspace, session, _) = ids();
    for exit_key in [AppKey::Escape, AppKey::CtrlC] {
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        assert!(state.has_live_pane());
        assert_eq!(state.overlay(), None);

        // Force the action modal over the live pane, then exit it.
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
        assert!(update(&mut state, AppEvent::Key(exit_key.clone())).is_empty());
        assert_eq!(
            state.route(),
            Route::Home(HomeMode::Closeup),
            "{exit_key:?}"
        );
        assert_eq!(state.overlay(), None, "{exit_key:?}");

        // A same-level live resample must not re-open the Closeup overlay.
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        assert_eq!(state.overlay(), None, "{exit_key:?}");
    }
}

/// A pane that never went live and loses its only (pending) tab restores the
/// empty Closeup. A failed launch also carries a safe reason there.
#[test]
fn failed_pane_launch_restores_empty_closeup_with_a_notice() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.overlay(), None);

    // The pending tab appears without changing overlay state.
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    assert_eq!(state.overlay(), None);

    // The launch failed: the pending tab is gone again, this time with a
    // safe reason attached.
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: false,
            error: Some("that agent CLI is not installed".to_owned()),
        },
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("that agent CLI is not installed")
    );
}

/// A clean pane exit restores the empty Closeup without a synthesized notice.
#[test]
fn clean_pane_exit_restores_empty_closeup_without_a_notice() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    assert_eq!(state.overlay(), None);
    assert!(state.notice().is_none());

    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: false,
            error: None,
        },
    );
    assert_eq!(state.overlay(), None);
    assert!(state.notice().is_none());
}

#[test]
fn terminal_launch_failure_opens_a_dismissible_error_dialog() {
    let (workspace, session, _) = ids();
    for dismiss in [AppKey::Escape, AppKey::Enter, AppKey::CtrlC] {
        let mut state = AppState::home(workspace, vec![session]);
        let effects = update(
            &mut state,
            AppEvent::TerminalLaunchFailed(Notice::new("shell executable was not found")),
        );

        assert!(effects.is_empty());
        assert_eq!(state.overlay(), Some(Overlay::TerminalLaunchError));
        assert_eq!(
            state
                .terminal_launch_error()
                .map(|notice| notice.message.as_str()),
            Some("shell executable was not found")
        );
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("shell executable was not found")
        );

        assert!(update(&mut state, AppEvent::Key(dismiss)).is_empty());
        assert_eq!(state.overlay(), None);
        assert!(state.terminal_launch_error().is_none());
    }
}

#[test]
fn terminal_launch_failure_does_not_replace_an_existing_modal() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);

    let _ = update(
        &mut state,
        AppEvent::TerminalLaunchFailed(Notice::new("daemon rejected the terminal")),
    );

    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.terminal_launch_error().is_none());
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("daemon rejected the terminal")
    );
}

#[test]
fn empty_home_refuses_root_agent_terminal_and_closeup_actions() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    assert_eq!(state.active, None);
    assert_eq!(Target::Root(workspace).session_id(), None);
    assert_eq!(Target::Session(session).session_id(), Some(session));

    // The public entry is inert without an active managed session.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    assert_eq!(state.overlay(), None);
    state.overlay = Some(Overlay::Closeup);
    let agent = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("agent".to_owned())),
    );
    assert!(agent.is_empty());

    state.overlay = Some(Overlay::Closeup);
    let terminal = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("terminal open".to_owned())),
    );
    assert!(terminal.is_empty());
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
    assert_eq!(state.selected(), Selection::Idle);

    // Workspace-global surfaces remain independent of managed navigation.
    assert!(matches!(
        update(&mut state, AppEvent::Key(AppKey::OpenEnvironment)).as_slice(),
        [Effect::LoadEnvironment {
            scope: EnvScope::Workspace
        }]
    ));
    state.overlay = None;
    assert!(matches!(
        update(&mut state, AppEvent::Key(AppKey::OpenDecisions)).as_slice(),
        [Effect::RefreshDecisions { workspace: actual }] if *actual == workspace
    ));
}

#[test]
fn closeup_agent_selects_an_installed_cli_and_refuses_the_rest() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));

    let launch = |state: &mut AppState, input: &str| {
        let _ = update(state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        update(
            state,
            AppEvent::Key(AppKey::SubmitCloseup(input.to_owned())),
        )
    };
    let profile = |effects: &[Effect]| match effects {
        [Effect::LaunchAgent { profile, .. }] => profile.as_ref().map(|id| id.as_str().to_owned()),
        _ => None,
    };

    // Every selectable CLI maps to its daemon profile; `sakana.ai` is
    // presented under its product name but launches the `sakana-ai` profile.
    for (input, expected) in [
        ("agent -m claude", "claude"),
        ("agent --model codex", "codex"),
        ("agent -m sakana.ai", "sakana-ai"),
    ] {
        assert_eq!(
            profile(&launch(&mut state, input)),
            Some(expected.to_owned()),
            "{input}"
        );
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some(format!("Requested agent {}", input.split(' ').next_back().unwrap()).as_str())
        );
    }

    // An omitted `-m` resolves the configured default and names it.
    state.set_agent_models(AvailableModels::all(), DefaultModel::SakanaAi);
    assert_eq!(
        profile(&launch(&mut state, "agent")),
        Some("sakana-ai".to_owned())
    );
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("Requested agent sakana.ai (default)")
    );

    // A CLI outside the vocabulary, and one that is not installed, are
    // refused with safe feedback while the modal stays open.
    state.set_agent_models(
        AvailableModels::new([DefaultModel::SakanaAi]),
        DefaultModel::SakanaAi,
    );
    for (input, message) in [
        ("agent -m gemini", "unknown agent CLI"),
        ("agent -m claude", "that agent CLI is not installed"),
        ("agent -x", "unknown agent flag"),
    ] {
        assert!(launch(&mut state, input).is_empty(), "{input}");
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some(message)
        );
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
    }

    // With no CLI installed even the default is refused rather than sent.
    state.set_agent_models(AvailableModels::default(), DefaultModel::OpenAi);
    assert!(launch(&mut state, "agent").is_empty());
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("the configured agent CLI is not installed")
    );
    assert_eq!(state.available_models(), AvailableModels::default());
    assert_eq!(state.default_model(), DefaultModel::OpenAi);
}

#[test]
fn closeup_env_opens_a_workspace_locked_editor_and_rejects_arguments() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));

    // `env` from Closeup opens this workspace's editor and requests a read,
    // replacing the Closeup overlay with the Environment editor.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("env".to_owned())),
    );
    assert_eq!(
        effects,
        vec![Effect::LoadEnvironment {
            scope: EnvScope::Workspace,
        }]
    );
    assert_eq!(state.overlay(), Some(Overlay::Environment));
    let editor = state.environment_editor().unwrap();
    assert_eq!(editor.scope(), EnvScope::Workspace);
    assert!(editor.is_loading());
    assert!(!editor.is_saving());

    // Once the read refluxes, Closeup uses the same multiline source and
    // Save focus interaction as Workspace Config.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::EnvironmentLoaded {
            scope: EnvScope::Workspace,
            entries: vec![entry("KEEP", "1")],
            inherited: vec![entry("GLOBAL", "hidden")],
        }),
    );
    let editor = state.environment_editor().unwrap();
    assert_eq!(editor.draft(), "KEEP=1");
    assert_eq!(editor.cursor(), "KEEP=1".len());
    assert!(!editor.is_save_focused());
    assert!(!editor.is_loading());
    assert!(!editor.is_saving());

    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::Paste("RUST_LOG=debug\r\nNEXT=2".to_owned())),
        )
        .is_empty()
    );
    assert_eq!(
        state.environment_editor().unwrap().draft(),
        "KEEP=1\nRUST_LOG=debug\nNEXT=2"
    );
    let end = state.environment_editor().unwrap().cursor();
    assert!(update(&mut state, AppEvent::Key(AppKey::Up)).is_empty());
    assert!(state.environment_editor().unwrap().cursor() < end);
    assert!(update(&mut state, AppEvent::Key(AppKey::Down)).is_empty());
    assert_eq!(state.environment_editor().unwrap().cursor(), end);
    assert!(update(&mut state, AppEvent::Key(AppKey::Tab)).is_empty());
    let editor = state.environment_editor().unwrap();
    assert_eq!(editor.scope(), EnvScope::Workspace);
    assert!(editor.is_save_focused());
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::SaveEnvironment {
            scope: EnvScope::Workspace,
            entries: vec![
                entry("KEEP", "1"),
                entry("NEXT", "2"),
                entry("RUST_LOG", "debug")
            ],
        }]
    );
    assert!(state.environment_editor().unwrap().is_saving());

    // Arguments (including `global`) are refused safely: the editor never
    // opens and the Closeup overlay stays up with a usage notice.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    for input in ["env workspace", "env global", "env extra"] {
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitCloseup(input.to_owned())),
            )
            .is_empty()
        );
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
        assert!(state.environment_editor().is_none());
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("env takes no arguments (usage: env)")
        );
    }
}

#[test]
fn closeup_environment_source_edits_at_the_cursor_and_keeps_validation_errors() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("env".to_owned())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::EnvironmentLoaded {
            scope: EnvScope::Workspace,
            entries: Vec::new(),
            inherited: Vec::new(),
        }),
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Char('é')));
    assert_eq!(state.environment_editor().unwrap().cursor(), 2);
    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    assert_eq!(state.environment_editor().unwrap().draft(), "é");
    let _ = update(&mut state, AppEvent::Key(AppKey::Right));
    let _ = update(&mut state, AppEvent::Key(AppKey::Right));
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    assert!(state.environment_editor().unwrap().draft().is_empty());

    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::Paste("MISSING_EQUALS".to_owned())),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
    assert!(!state.environment_editor().unwrap().is_save_focused());
    let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    let editor = state.environment_editor().unwrap();
    assert_eq!(editor.draft(), "MISSING_EQUALS");
    assert!(!editor.is_save_focused());
    assert_eq!(
        editor.error().unwrap().message.as_str(),
        "line 1: expected NAME=value"
    );

    let editor = state.environment_editor.as_mut().unwrap();
    editor.source.replace("OK=1");
    editor.error = None;
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::SaveEnvironment)),
        vec![Effect::SaveEnvironment {
            scope: EnvScope::Workspace,
            entries: vec![entry("OK", "1")],
        }]
    );
    assert!(
        update(&mut state, AppEvent::Key(AppKey::SaveEnvironment)).is_empty(),
        "a save in flight must not be submitted twice"
    );
    assert!(update(&mut state, AppEvent::Key(AppKey::Tab)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Up)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Down)).is_empty());
}

#[test]
fn closeup_environment_ctrl_s_saves_the_workspace_source() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("env".to_owned())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::EnvironmentLoaded {
            scope: EnvScope::Workspace,
            entries: vec![entry("RUST_LOG", "debug")],
            inherited: Vec::new(),
        }),
    );

    // A completion not initiated by this editor refreshes its projection
    // without closing the modal. The initiated completion below closes it.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::EnvironmentSaved {
            scope: EnvScope::Workspace,
            entries: vec![entry("RUST_LOG", "debug")],
            inherited: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Environment));
    assert!(!state.environment_editor().unwrap().is_saving());

    let save = update(&mut state, AppEvent::Key(AppKey::SaveRoles));
    assert_eq!(
        save,
        vec![Effect::SaveEnvironment {
            scope: EnvScope::Workspace,
            entries: vec![entry("RUST_LOG", "debug")],
        }]
    );
    assert_eq!(state.overlay(), Some(Overlay::Environment));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::EnvironmentSaved {
            scope: EnvScope::Workspace,
            entries: vec![entry("RUST_LOG", "debug")],
            inherited: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.environment_editor().is_none());
}

#[test]
fn closeup_environment_source_reports_each_invalid_line_shape_and_limits() {
    for (source, expected) in [
        ("\nMISSING", "line 2: expected NAME=value"),
        ("1BAD=value", "line 1: invalid variable name"),
        ("EMPTY=", "line 1: remove the line to unset it"),
        ("NUL=a\0b", "line 1: values cannot contain NUL"),
    ] {
        assert_eq!(parse_environment_source(source), Err(expected.to_owned()));
    }

    let over_limit = (0..=usagi_core::domain::settings::MAX_ENV_BINDINGS)
        .map(|index| format!("KEY_{index}=value"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        parse_environment_source(&over_limit)
            .unwrap_err()
            .contains("binding limit")
    );
}

#[test]
fn a_live_pane_that_releases_the_foreground_takes_its_modal_state_with_it() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));

    // A pane going live releases the Closeup foreground. Whatever modal held it
    // must go with it: input is routed by the foreground alone, so a surviving
    // modal would be drawn while every key reached Home instead.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
    assert_eq!(state.overlay(), Some(Overlay::Preview));
    assert!(state.preview_overlay().is_some());
    assert_eq!(
        update(&mut state, AppEvent::LivePaneAvailability(true)),
        vec![Effect::CancelPreview],
        "closing the preview this way also ends the scan behind it"
    );
    assert_eq!(state.overlay(), None);
    assert!(state.preview_overlay().is_none());
}

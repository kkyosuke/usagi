//! session の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn workflow_panels_follow_authoritative_session_removal() {
    let workspace = WorkspaceId::new();
    let first = SessionId::new();
    let second = SessionId::new();
    let mut state = AppState::home(workspace, vec![first, second]);
    state.workflows.entry(first).or_default();
    state.workflows.entry(second).or_default();
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Sessions(vec![second])),
    );
    assert!(state.workflow_panel(first).is_none());
    assert!(state.workflow_panel(second).is_some());
}

#[test]
fn closeup_sidebar_pr_badge_click_opens_the_clicked_background_sessions_modal() {
    let workspace = WorkspaceId::new();
    let active = SessionId::new();
    let clicked = SessionId::new();
    let target = Target::Session(clicked);
    let mut state = sized_home(workspace, vec![active, clicked], 100, 30);
    let pr = pr_link(41);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr.clone()],
        }),
    );
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), Selection::Target(Target::Session(active)));

    // The background session's metadata is row 6 (two chrome rows, the active
    // session's three lines, then this session's summary line). Its
    // three-cell badge is flush right in the 36-cell sidebar, so the last
    // cell must open that session's PRs without changing the active sidebar
    // selection or contributing to a double-click.
    assert_eq!(
        update(
            &mut state,
            AppEvent::Pointer {
                column: 35,
                row: 6,
                at: std::time::Duration::ZERO,
            },
        ),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().target(), target);
    assert_eq!(state.pr_overlay().unwrap().prs(), std::slice::from_ref(&pr));
    assert_eq!(state.selected(), Selection::Target(Target::Session(active)));
}

#[test]
fn session_pointer_single_click_selects_and_double_click_matches_enter() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = sized_home(workspace, vec![session], 100, 30);

    assert_eq!(
        click_at(&mut state, 5, 2, 1_000),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(state.active(), Some(session));
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
    assert_eq!(state.overlay(), None);

    // The inclusive 400ms boundary is the same activation path as Enter.
    assert_eq!(
        click_at(&mut state, 5, 2, 1_400),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(state.active(), Some(session));
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));
    assert_eq!(state.overlay(), None);
}

#[test]
fn session_pointer_outside_window_starts_a_new_pair_and_regressed_time_is_safe() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = click_at(&mut state, 5, 2, 1_000);
    let _ = click_at(&mut state, 5, 2, 1_401);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
    let _ = click_at(&mut state, 5, 2, 1_200);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
    let _ = click_at(&mut state, 5, 2, 1_600);
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));
}

#[test]
fn non_session_pointer_hits_invalidate_the_pending_session_press() {
    let (workspace, session, _) = ids();
    // `+ new session`（row 5）、行の外、chrome 行、sidebar の外。
    for (column, row) in [(5, 5), (5, 8), (5, 1), (90, 4)] {
        let mut state = sized_home(workspace, vec![session], 100, 30);
        let _ = click_at(&mut state, 5, 2, 1_000);
        let _ = click_at(&mut state, column, row, 1_100);
        let _ = click_at(&mut state, 5, 2, 1_200);
        assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
    }
}

#[test]
fn another_session_and_scrolled_same_cell_do_not_activate() {
    let workspace = WorkspaceId::new();
    let sessions: Vec<SessionId> = (0..6).map(|_| SessionId::new()).collect();
    let mut other = sized_home(workspace, sessions[..2].to_vec(), 100, 30);
    let _ = click_at(&mut other, 5, 2, 1_000);
    // 2 件目の session の先頭行。3 行 footprint なので row 5 から始まる。
    let _ = click_at(&mut other, 5, 5, 1_100);
    assert!(matches!(other.route(), Route::Home(HomeMode::Switch)));

    let mut scrolled = sized_home(workspace, sessions, 100, 14);
    let mut tail = scrolled.clone();
    tail.selected = Selection::NewSession;
    let (row, first, second) = (2_u16..14)
        .find_map(|row| {
            match (
                scrolled.sidebar_selection_at(5, row),
                tail.sidebar_selection_at(5, row),
            ) {
                (
                    Some(Selection::Target(Target::Session(first))),
                    Some(Selection::Target(Target::Session(second))),
                ) if first != second => Some((row, first, second)),
                _ => None,
            }
        })
        .expect("scrolling replaces a visible session cell with another identity");
    let _ = click_at(&mut scrolled, 5, row, 1_000);
    scrolled.selected = Selection::NewSession;
    assert_ne!(first, second);
    let _ = click_at(&mut scrolled, 5, row, 1_100);
    assert!(matches!(scrolled.route(), Route::Home(HomeMode::Switch)));
}

#[test]
fn session_snapshot_invalidates_pending_press_even_when_identity_remains() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = click_at(&mut state, 5, 2, 1_000);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Sessions(vec![session])),
    );
    let _ = click_at(&mut state, 5, 2, 1_100);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
    let _ = click_at(&mut state, 5, 2, 1_500);
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));
}

#[test]
fn create_session_form_edits_the_name_only_and_defaults_profile_and_model() {
    let mut form = CreateSessionForm::default();
    assert_eq!(form.name(), "");
    assert!(form.error().is_none());

    assert!(required_create_value(" ", "required").is_err());
    assert_eq!(required_create_value(" name ", "required").unwrap(), "name");

    for character in "sessio".chars() {
        form.push(character);
    }
    form.backspace();
    form.push('o');
    form.push('n');

    let request = form.request().unwrap();
    assert_eq!(request.name, "session");
    // profile / model are no longer part of the create flow: the intent always
    // defers to the daemon's workspace default policy.
    assert!(request.profile.is_none());
    assert!(request.model.is_none());
}

#[test]
fn role_catalog_defaults_picker_and_create_intent_to_role_id_only() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let coder = RoleId::new("coder").unwrap();
    let reviewer = RoleId::new("reviewer").unwrap();
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionRoleCatalog(SessionRoleCatalog {
            roles: vec![
                RoleChoice {
                    id: coder.clone(),
                    summary: "Code".into(),
                },
                RoleChoice {
                    id: reviewer.clone(),
                    summary: "Review".into(),
                },
            ],
            default: Some(coder),
        })),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionBranchCatalog(SessionBranchCatalog {
            branches: vec![
                BranchChoice {
                    label: "local:main".into(),
                    refname: "refs/heads/main".into(),
                },
                BranchChoice {
                    label: "remote:origin/(default)".into(),
                    refname: "refs/remotes/origin/HEAD".into(),
                },
                BranchChoice {
                    label: "remote:origin/main".into(),
                    refname: "refs/remotes/origin/main".into(),
                },
            ],
            default: Some("refs/heads/main".into()),
        })),
    );
    assert_eq!(state.role_catalog().roles.len(), 2);
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
    assert_eq!(state.create_session_form().unwrap().roles().len(), 2);
    assert_eq!(
        state
            .create_session_form()
            .unwrap()
            .selected_role()
            .unwrap()
            .id
            .as_str(),
        "coder"
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
    state.create_session.as_mut().unwrap().move_role(true);
    let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
    for character in "feature".chars() {
        let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
    }
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert!(
        matches!(effects.as_slice(), [Effect::CreateSession { intent, .. }]
            if intent.role_id.as_ref() == Some(&reviewer)
                && intent.base_ref.as_deref() == Some("refs/remotes/origin/HEAD")
                && intent.profile.is_none() && intent.model.is_none())
    );

    let mut empty = CreateSessionForm::new(Vec::new());
    empty.move_role(true);
    assert!(empty.selected_role().is_none());
    empty.move_branch(true);
    assert!(empty.selected_branch().is_none());
}

#[test]
fn create_session_form_defers_the_empty_name_error_to_submit() {
    // While typing nothing, the empty name is "in progress", not an error.
    let mut form = CreateSessionForm::new(Vec::new());
    form.push(' ');
    assert!(
        form.error().is_none(),
        "whitespace-only is not a live error"
    );
    // Submitting an effectively empty name surfaces the required-name error and
    // keeps the draft so the user can keep typing.
    let error = form.request().unwrap_err();
    assert_eq!(error.message, "session name is required");
    assert_eq!(form.name(), " ", "draft is preserved after a failed submit");
}

#[test]
fn create_session_form_rejects_invalid_characters_while_typing() {
    let mut form = CreateSessionForm::new(Vec::new());
    for character in "ok".chars() {
        form.push(character);
    }
    assert!(form.error().is_none());
    form.push('/');
    assert_eq!(form.error().unwrap().message, "invalid character");
    // Submitting keeps the draft and refuses to build a request.
    assert!(form.request().is_err());
    assert_eq!(form.name(), "ok/");
    // Fixing the input clears the error and lets the request through.
    form.backspace();
    assert!(form.error().is_none());
    assert_eq!(form.request().unwrap().name, "ok");
}

#[test]
fn create_session_form_rejects_names_longer_than_the_limit() {
    let mut form = CreateSessionForm::new(Vec::new());
    for _ in 0..=MAX_SESSION_NAME_LEN {
        form.push('a');
    }
    assert_eq!(form.error().unwrap().message, "name too long (max 64)");
    assert!(form.request().is_err());
    // A name exactly at the limit is accepted.
    form.backspace();
    assert!(form.error().is_none());
    assert_eq!(form.name().chars().count(), MAX_SESSION_NAME_LEN);
    assert_eq!(
        form.request().unwrap().name.chars().count(),
        MAX_SESSION_NAME_LEN
    );
}

#[test]
fn create_session_form_rejects_a_duplicate_of_a_displayed_session() {
    let mut form = CreateSessionForm::new(vec!["alpha".to_owned()]);
    for character in "alpha".chars() {
        form.push(character);
    }
    assert_eq!(form.error().unwrap().message, "session name already exists");
    assert!(form.request().is_err());
    assert_eq!(form.name(), "alpha", "draft is preserved");
    // A distinct name is accepted; the duplicate check is against the exact name.
    form.push('-');
    form.push('2');
    assert!(form.error().is_none());
    assert_eq!(form.request().unwrap().name, "alpha-2");
}

#[test]
fn open_create_session_seeds_the_form_with_displayed_names() {
    let workspace = WorkspaceId::new();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionNames(vec!["alpha".to_owned()])),
    );
    // Down reaches `+ new session`; Enter opens the form seeded with the names.
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    for character in "alpha".chars() {
        let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
    }
    let form = state.create_session_form().unwrap();
    assert_eq!(form.error().unwrap().message, "session name already exists");
}

#[test]
fn open_create_session_revalidates_when_a_conflict_becomes_known() {
    let workspace = WorkspaceId::new();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    for character in "alpha".chars() {
        let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
    }
    assert!(state.create_session_form().unwrap().error().is_none());

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionNames(vec!["alpha".to_owned()])),
    );
    let form = state.create_session_form().unwrap();
    assert_eq!(form.name(), "alpha", "the draft is preserved");
    assert_eq!(form.error().unwrap().message, "session name already exists");

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionNames(Vec::new())),
    );
    assert!(
        state.create_session_form().unwrap().error().is_none(),
        "removing the conflict also clears the live error"
    );
}

#[test]
fn home_starts_with_first_session_or_neutral_selection_when_empty() {
    let (workspace, first, second) = ids();
    let state = AppState::home(workspace, vec![first, second]);
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
    assert_eq!(state.selected(), Selection::Target(Target::Session(first)));
    assert_eq!(state.active(), Some(first));
    assert_eq!(state.sessions(), &[first, second]);

    let empty = AppState::home(workspace, Vec::new());
    assert_eq!(empty.selected(), Selection::Idle);
    assert_eq!(empty.active(), None);

    let mut empty = empty;
    assert!(update(&mut empty, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(empty.overlay(), None);
    assert!(update(&mut empty, AppEvent::Key(AppKey::Char('t'))).is_empty());
    assert_eq!(empty.selected(), Selection::Idle);
    assert_eq!(empty.overlay(), None);
    let _ = update(&mut empty, AppEvent::Key(AppKey::Down));
    assert_eq!(empty.selected(), Selection::NewSession);
    assert!(update(&mut empty, AppEvent::Key(AppKey::Char('t'))).is_empty());
    assert_eq!(empty.overlay(), Some(Overlay::CreateSession));
}

#[test]
fn session_navigation_cycles_usable_rows_and_keeps_closeup_active() {
    let workspace = WorkspaceId::new();
    let first = SessionId::new();
    let failed = SessionId::new();
    let third = SessionId::new();
    let mut state = AppState::home(workspace, vec![first, failed, third]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([
            (first, lifecycle(SessionLifecycle::Available)),
            (failed, lifecycle(SessionLifecycle::Failed)),
            (third, lifecycle(SessionLifecycle::Available)),
        ]))),
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    let _ = update(&mut state, AppEvent::Key(AppKey::NextSession));
    assert_eq!(state.selected(), Selection::Target(Target::Session(third)));
    assert_eq!(state.active(), Some(third));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));

    let _ = update(&mut state, AppEvent::Key(AppKey::NextSession));
    assert_eq!(
        state.active(),
        Some(first),
        "next wraps past the failed row"
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::PreviousSession));
    assert_eq!(state.active(), Some(third), "previous wraps the other way");
}

#[test]
fn session_navigation_moves_only_the_switch_cursor_and_yields_to_overlays() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut state, AppEvent::Key(AppKey::NextSession));
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
    assert_eq!(state.active(), Some(first));
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));

    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
    let before = (state.selected(), state.active(), state.route());
    let _ = update(&mut state, AppEvent::Key(AppKey::PreviousSession));
    assert_eq!((state.selected(), state.active(), state.route()), before);
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));

    let mut closeup_actions = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut closeup_actions, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut closeup_actions, AppEvent::Key(AppKey::Enter));
    assert_eq!(closeup_actions.overlay(), Some(Overlay::Closeup));
    let before = (
        closeup_actions.selected(),
        closeup_actions.active(),
        closeup_actions.route(),
    );
    let _ = update(&mut closeup_actions, AppEvent::Key(AppKey::NextSession));
    assert_eq!(
        (
            closeup_actions.selected(),
            closeup_actions.active(),
            closeup_actions.route(),
        ),
        before
    );
}

#[test]
fn session_navigation_uses_directional_edges_without_an_anchor() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    state.selected = Selection::NewSession;
    state.active = None;

    let _ = update(&mut state, AppEvent::Key(AppKey::NextSession));
    assert_eq!(state.selected(), Selection::Target(Target::Session(first)));

    state.selected = Selection::NewSession;
    state.active = None;
    let _ = update(&mut state, AppEvent::Key(AppKey::PreviousSession));
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));

    let mut empty = AppState::home(workspace, Vec::new());
    let before = (empty.selected(), empty.active(), empty.route());
    assert!(update(&mut empty, AppEvent::Key(AppKey::NextSession)).is_empty());
    assert_eq!((empty.selected(), empty.active(), empty.route()), before);
}

#[test]
fn ctrl_a_opens_a_typed_create_form_and_lands_only_without_later_interaction() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
    assert_eq!(state.active(), None);
    assert_eq!(state.selected(), Selection::NewSession);
    assert_eq!(state.overlay(), Some(Overlay::CreateSession));
    assert_eq!(state.create_session_form().unwrap().name(), "");
    // Home / Tab while the name-only form owns input must not retrigger create
    // nor edit any removed field.
    let _ = update(&mut state, AppEvent::Key(AppKey::Home));
    let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
    assert_eq!(state.overlay(), Some(Overlay::CreateSession));
    for key in [
        AppKey::Char('w'),
        AppKey::Char('o'),
        AppKey::Char('r'),
        AppKey::Char('k'),
    ] {
        let _ = update(&mut state, AppEvent::Key(key));
    }
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert!(matches!(
        &effects[..],
        [Effect::CreateSession { workspace: actual_workspace, token: PendingToken(1), intent, .. }]
            if *actual_workspace == workspace
                && intent.name == "work"
                && intent.profile.is_none()
                && intent.model.is_none()
    ));
    assert_eq!(state.pending().len(), 1);
    let token = state.pending()[0].token;

    let created = SessionId::new();
    assert!(
        update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token,
                succeeded: true,
                created: Some(created),
                notice: Some(Notice::new("created")),
            }),
        )
        .is_empty()
    );
    assert!(state.pending().is_empty());
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("created")
    );
    assert_eq!(state.active(), Some(created));
    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(created))
    );
    assert_eq!(state.overlay(), None);

    let effects = update(
        &mut state,
        AppEvent::OperationResult(OperationResult {
            token: PendingToken(99),
            succeeded: false,
            created: None,
            notice: Some(Notice::new("safe failure")),
        }),
    );
    assert!(effects.is_empty());
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("safe failure")
    );
}

#[test]
fn a_failed_create_opens_the_error_dialog_with_only_the_safe_message() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let token = submit_create(&mut state, &['a', 'p', 'i']);
    // Submitting closes the form and leaves no overlay open.
    assert_eq!(state.overlay(), None);
    assert!(state.create_session_form().is_none());
    assert_eq!(state.pending().len(), 1);

    let effects = update(
        &mut state,
        AppEvent::OperationResult(OperationResult {
            token,
            succeeded: false,
            created: None,
            notice: Some(Notice::new("worktree path already exists")),
        }),
    );
    assert!(effects.is_empty());
    // The pending row is cleared and the dialog carries the safe message.
    assert!(state.pending().is_empty());
    assert_eq!(state.overlay(), Some(Overlay::CreateSessionError));
    assert_eq!(
        state
            .create_session_error()
            .map(|notice| notice.message.as_str()),
        Some("worktree path already exists")
    );
    // No half-created state leaks: sidebar rows and active target are unchanged.
    assert!(state.sessions().is_empty());
    assert_eq!(state.active(), None);
}

#[test]
fn dismissing_the_create_error_dialog_returns_to_home_without_residue() {
    let (workspace, _, _) = ids();
    for dismiss in [AppKey::Escape, AppKey::Enter, AppKey::CtrlC] {
        let mut state = AppState::home(workspace, Vec::new());
        let token = submit_create(&mut state, &['x']);
        let _ = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new("daemon unavailable")),
            }),
        );
        assert_eq!(state.overlay(), Some(Overlay::CreateSessionError));

        let effects = update(&mut state, AppEvent::Key(dismiss));
        assert!(effects.is_empty());
        assert_eq!(state.overlay(), None);
        assert!(state.create_session_error().is_none());
        assert!(state.create_session_form().is_none());
        // Dismissal leaves the resident Home route intact.
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
    }
}

#[test]
fn a_create_failure_keeps_the_notice_fallback_while_another_overlay_is_open() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let token = submit_create(&mut state, &['y']);
    // The user opens the quit confirmation before the create result returns.
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));

    let _ = update(
        &mut state,
        AppEvent::OperationResult(OperationResult {
            token,
            succeeded: false,
            created: None,
            notice: Some(Notice::new("safe failure")),
        }),
    );
    // The open overlay is not clobbered; the message stays a plain notice.
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
    assert!(state.create_session_error().is_none());
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("safe failure")
    );
}

#[test]
fn closeup_pane_navigation_chords_keep_create_and_action_scopes_separate() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    // Switch keeps Ctrl-A as the IME-safe create shortcut and ignores Ctrl-O.
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlO)).is_empty());
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
    assert_eq!(state.overlay(), Some(Overlay::CreateSession));
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));

    // On an active session in Closeup, Ctrl-A owns the target action surface,
    // and must not resurrect the workspace-level create form.
    // Ctrl-A moved the cursor to `+ new session`; return it to the session.
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
    assert_eq!(state.overlay(), Some(Overlay::Closeup));
    assert!(state.create_session_form().is_none());

    // Ctrl-O is the Closeup-to-Switch pane-navigation transition, and it
    // clears the forced action overlay on the way out.
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlO)).is_empty());
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
    assert_eq!(state.overlay(), None);
}

#[test]
fn invalid_create_stays_open_and_late_success_does_not_move_after_interaction() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::CreateSession));
    assert_eq!(
        state
            .create_session_form()
            .and_then(CreateSessionForm::error)
            .map(|error| error.message.as_str()),
        Some("session name is required")
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('a')));
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    let token = match &effects[..] {
        [Effect::CreateSession { token, .. }] => *token,
        _ => panic!("expected create effect"),
    };
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    let created = SessionId::new();
    let _ = update(
        &mut state,
        AppEvent::OperationResult(OperationResult {
            token,
            succeeded: true,
            created: Some(created),
            notice: None,
        }),
    );
    assert_eq!(state.active(), None);
    assert_ne!(
        state.selected(),
        Selection::Target(Target::Session(created))
    );
}

#[test]
fn snapshot_reconciles_missing_selected_and_active_sessions_by_display_order() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Sessions(vec![second])),
    );
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
    assert_eq!(state.active(), Some(second));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Sessions(Vec::new())),
    );
    assert_eq!(state.selected(), Selection::Idle);
    assert_eq!(state.active(), None);
}

#[test]
fn phase_projection_rejects_other_workspaces_and_removed_sessions() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let foreign = runtime(WorkspaceId::new(), session);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RuntimePhase {
            runtime: foreign,
            phase: AgentPhase::Running,
        }),
    );
    assert!(state.runtimes().is_empty());

    let known = runtime(workspace, session);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RuntimePhase {
            runtime: known,
            phase: AgentPhase::Running,
        }),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Sessions(Vec::new())),
    );
    assert!(state.runtimes().is_empty());
}

#[test]
fn reconnect_feedback_refreshes_prs_for_the_current_session_set() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);

    for feedback in [Feedback::Reconnected, Feedback::ResyncRequired] {
        assert_eq!(
            update(
                &mut state,
                AppEvent::Backend(BackendEvent::Feedback(feedback.clone())),
            ),
            vec![Effect::SyncPullRequestTargets {
                sessions: vec![first, second],
            }]
        );
        assert_eq!(state.feedback(), Some(&feedback));
    }
}

#[test]
fn switch_ctrl_x_force_removes_and_plain_x_is_inert() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);

    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session: first,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }]
    );
    assert_eq!(state.selected(), Selection::Target(Target::Session(first)));

    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    for key in [AppKey::Char('x'), AppKey::Char('X')] {
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
    }
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session: second,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }]
    );
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
}

#[test]
fn switch_ctrl_x_force_removes_regular_sessions_and_purges_integrity_orphans() {
    let (workspace, session, _) = ids();
    let mut empty_state = AppState::home(workspace, Vec::new());
    assert!(
        update(&mut empty_state, AppEvent::Key(AppKey::CtrlX)).is_empty(),
        "a non-session selection must not become a purge target"
    );

    let mut state = AppState::home(workspace, vec![session]);

    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }],
        "an available session is force-removed without a purge acknowledgement"
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Delete),
                failure_summary: Some("remove failed".to_owned()),
            },
        )]))),
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }],
        "an ordinary delete failure retries the same force removal"
    );

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Integrity),
                failure_summary: Some("orphan session".to_owned()),
            },
        )]))),
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: true,
        }]
    );

    state.sessions.clear();
    assert!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)).is_empty(),
        "a stale selected identity must not become a purge target"
    );

    state.sessions.push(session);
    state.session_lifecycles.insert(
        session,
        SessionLifecycleProjection {
            lifecycle: SessionLifecycle::Available,
            failure_stage: Some(FailureStage::Integrity),
            failure_summary: Some("incoherent projection".to_owned()),
        },
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }],
        "an integrity stage without the failed lifecycle is not a purge target"
    );
}

#[test]
fn deleting_session_keeps_the_cursor_without_accepting_another_remove() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            lifecycle(SessionLifecycle::Deleting),
        )]))),
    );

    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(session))
    );
    for key in [AppKey::CtrlX, AppKey::Char('x'), AppKey::Char('X')] {
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert_eq!(
            state.selected(),
            Selection::Target(Target::Session(session))
        );
    }
}

#[test]
fn failed_session_is_not_normally_attachable_but_retained_panes_are_reachable() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    assert!(
        update(
            &mut state,
            AppEvent::RetainedPaneActivated(Target::Root(workspace)),
        )
        .is_empty()
    );
    assert!(
        update(
            &mut state,
            AppEvent::RetainedPaneActivated(Target::Session(session)),
        )
        .is_empty(),
        "an available session cannot enter the recovery-only path"
    );
    // The daemon reports the session as Failed.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            lifecycle(SessionLifecycle::Failed),
        )]))),
    );
    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(session))
    );

    // Activation does not attach a Failed row (`can_use=false`): no effect,
    // no active managed target remains, and the route never enters Closeup.
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(state.active(), None);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));

    // The presentation runtime emits this only after finding an existing
    // pane tab for this exact failed target. It opens the retained terminal
    // surface without making the failed checkout usable for new launches.
    assert!(
        update(
            &mut state,
            AppEvent::RetainedPaneActivated(Target::Session(session)),
        )
        .is_empty()
    );
    assert_eq!(state.active(), Some(session));
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));

    // Removal is still offered (`can_remove=true`).
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlO));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }]
    );
}

#[test]
fn force_remove_confirmation_handles_no_and_unsupported_keys_and_a_missing_target() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Delete),
                failure_summary: None,
            },
        )]))),
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert!(update(&mut state, AppEvent::Key(AppKey::Home)).is_empty());
    assert_eq!(state.force_remove_confirmation(), Some((session, true)));
    assert!(
        update(&mut state, AppEvent::Key(AppKey::Char('n'))).is_empty(),
        "No closes the prompt without removing the session"
    );
    assert_eq!(state.overlay(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    state.force_remove_confirmation = None;
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(state.overlay(), None);
}

#[test]
fn force_remove_confirmation_reconciles_a_changed_failure_without_clobbering_an_overlay() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Delete),
                failure_summary: None,
            },
        )]))),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Delete),
                failure_summary: Some("refreshed detail".to_owned()),
            },
        )]))),
    );
    assert_eq!(state.force_remove_confirmation(), Some((session, true)));
    state.overlay = Some(Overlay::Daemon);

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Create),
                failure_summary: None,
            },
        )]))),
    );

    assert_eq!(state.force_remove_confirmation(), None);
    assert_eq!(state.overlay(), Some(Overlay::Daemon));
}

#[test]
fn an_available_session_stays_attachable_after_a_lifecycle_refresh() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            lifecycle(SessionLifecycle::Available),
        )]))),
    );
    // An Available row attaches as before: the route enters Closeup and the
    // session becomes the active target.
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.active(), Some(session));
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));
}

#[test]
fn overview_session_commands_use_typed_lifecycle_effects() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let create = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("session create feature-x".into())),
    );
    assert!(matches!(
        &create[..],
        [Effect::CreateSession { workspace: actual, intent, .. }]
            if *actual == workspace && intent.name == "feature-x" && intent.profile.is_none() && intent.model.is_none()
    ));
    assert_eq!(state.overlay(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("session list".into())),
        ),
        vec![Effect::RefreshSessions { workspace }]
    );

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionNames(vec!["feature-x".into()])),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let resume = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("session resume feature-x".into())),
    );
    assert!(matches!(
        resume.as_slice(),
        [Effect::ResumeAgent {
            workspace: actual_workspace,
            session: actual_session,
            ..
        }] if *actual_workspace == workspace && *actual_session == session
    ));

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("session sleep feature-x".into())),
        ),
        vec![Effect::SleepSession { workspace, session }]
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("session sleep missing".into())),
        )
        .is_empty()
    );
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("session was not found")
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("session resume missing".into())),
        )
        .is_empty()
    );
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("session was not found")
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview(
                "session remove feature-x --force".into(),
            )),
        ),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }]
    );
    assert_eq!(state.overlay(), None);
}

#[test]
fn closeup_registry_dispatches_agent_and_validated_session_remove() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("agent codex".to_owned())),
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::LaunchAgent {
            workspace: effect_workspace,
            session: effect_session,
            profile: Some(profile),
            ..
        }] if *effect_workspace == workspace && *effect_session == Some(session) && profile.as_str() == "codex"
    ));

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("close --force".to_owned())),
        ),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }]
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("chat".to_owned())),
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Closeup));
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("unknown closeup command: \"chat\"")
    );
}

/// A rabbit is a stable session: clicking it closes the garden and enters
/// that session's existing Closeup, with no double-click wait.
#[test]
fn clicking_a_usagi_visits_its_session_in_one_press() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 100, 30);
    state.overlay = Some(Overlay::Garden);

    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Visit {
                workspace,
                session: second,
                agent: None,
            })
        )
        .is_empty()
    );
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
    assert_eq!(state.active(), Some(second));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    // The garden is gone and the tabless Closeup opens on its empty pane.
    assert_eq!(state.overlay(), None);
}

#[test]
fn another_projects_garden_plot_closes_without_targeting_a_local_session() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 100, 30);
    let (selected, active) = (state.selected(), state.active());
    state.overlay = Some(Overlay::Garden);

    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Visit {
                workspace: WorkspaceId::new(),
                session: second,
                agent: None,
            })
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), selected);
    assert_eq!(state.active(), active);
}

#[test]
fn a_deck_visit_opens_a_fresh_workspaces_stable_session() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 100, 30);

    assert!(update(&mut state, AppEvent::VisitSession(second)).is_empty());
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
    assert_eq!(state.active(), Some(second));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
}

#[test]
fn a_project_return_focuses_the_stable_session_without_opening_closeup() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 100, 30);

    assert!(update(&mut state, AppEvent::FocusSession(second)).is_empty());
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
    assert_eq!(state.active(), Some(first));
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));

    assert!(update(&mut state, AppEvent::FocusSession(SessionId::new())).is_empty());
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
}

#[test]
fn decisions_are_workspace_fenced_retryable_and_removed_only_on_confirmation() {
    let workspace = WorkspaceId::new();
    let foreign = WorkspaceId::new();
    let decision = pending_decision(workspace);
    let mut state = AppState::home(workspace, Vec::new());
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::OpenDecisions)),
        vec![Effect::RefreshDecisions { workspace }]
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace: foreign,
            decisions: vec![pending_decision(foreign)],
        }),
    );
    assert!(state.decisions().is_empty());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions: vec![decision.clone()],
        }),
    );
    assert_eq!(state.unread_decision_ids().len(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDecisions));
    assert!(state.unread_decision_ids().is_empty());
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::SubmitDecision)),
        vec![Effect::ResolveDecision {
            workspace,
            decision_id: decision.decision_id,
            answer: UserDecisionAnswer::Option {
                option_id: "safe".into()
            }
        }]
    );
    assert_eq!(state.decisions(), std::slice::from_ref(&decision));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DecisionError {
            workspace,
            decision_id: decision.decision_id,
            error: SafeError {
                message: SafeMessage::new("try again"),
                error_id: "resolve".into(),
            },
        }),
    );
    assert_eq!(state.decisions(), std::slice::from_ref(&decision));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DecisionResolved {
            workspace,
            decision_id: decision.decision_id,
        }),
    );
    assert!(state.decisions().is_empty());
}

#[test]
fn cleanup_queue_admits_only_idle_sessions_whose_visible_prs_are_all_merged() {
    let (workspace, mixed, ready) = ids();
    let mut state = AppState::home(workspace, vec![mixed, ready]);
    observe_prs(&mut state, mixed, 1, vec![merged_pr_link(1), pr_link(2)]);
    observe_prs(&mut state, ready, 1, vec![merged_pr_link(3)]);
    state.overlay = Some(Overlay::Overview);

    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("session cleanup".to_owned()))
        ),
        vec![Effect::SyncPullRequestTargets {
            sessions: vec![mixed, ready]
        }]
    );
    assert_eq!(state.overlay(), Some(Overlay::CleanupQueue));
    assert_eq!(state.cleanup_queue().unwrap().candidates(), &[ready]);

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RuntimePhase {
            runtime: runtime(workspace, ready),
            phase: AgentPhase::Running,
        }),
    );
    assert!(state.cleanup_queue().unwrap().candidates().is_empty());
}

#[test]
fn cleanup_queue_revalidates_pr_state_and_pauses_on_a_remove_error() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    observe_prs(&mut state, session, 1, vec![merged_pr_link(1)]);
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("session cleanup".to_owned())),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));

    observe_prs(&mut state, session, 2, vec![pr_link(1)]);
    assert!(state.cleanup_queue().unwrap().candidates().is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());

    observe_prs(&mut state, session, 3, vec![merged_pr_link(1)]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    assert!(matches!(
        update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
        [Effect::RemoveSession { session: target, .. }] if *target == session
    ));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Notice(Notice::new("worktree is dirty"))),
    );
    let queue = state.cleanup_queue().unwrap();
    assert_eq!(queue.in_flight(), None);
    assert!(queue.selected().is_empty());
    assert_eq!(queue.feedback().unwrap().message, "worktree is dirty");
}

#[test]
fn remove_selector_starts_at_the_current_row_and_serializes_forced_removals() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));

    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview(
                "session remove --select --force".to_owned()
            ))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::RemoveSessions));
    let queue = state.remove_queue().unwrap();
    assert_eq!(queue.candidates(), &[first, second]);
    assert_eq!(queue.cursor(), 1);
    assert!(queue.force());

    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('k')));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    assert_eq!(
        state.remove_queue().unwrap().selected(),
        &BTreeSet::from([first, second])
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::RemoveSession {
            workspace,
            session: first,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }]
    );

    assert_eq!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(vec![second]))
        ),
        vec![
            Effect::SyncPullRequestTargets {
                sessions: vec![second]
            },
            Effect::RemoveSession {
                workspace,
                session: second,
                force: true,
                force_delete_branch: true,
                purge_orphan: false,
            },
        ]
    );
    assert_eq!(state.remove_queue().unwrap().in_flight(), Some(second));

    assert_eq!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(Vec::new()))
        ),
        vec![Effect::SyncPullRequestTargets {
            sessions: Vec::new()
        }]
    );
    let queue = state.remove_queue().unwrap();
    assert!(queue.candidates().is_empty());
    assert_eq!(queue.in_flight(), None);
    assert_eq!(queue.feedback().unwrap().message, "removal complete");
}

#[test]
fn remove_selector_revalidates_lifecycle_and_pauses_after_failure() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("session remove -s".to_owned())),
    );

    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(
        state.remove_queue().unwrap().feedback().unwrap().message,
        "select sessions with Space"
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('j')));
    assert_eq!(state.remove_queue().unwrap().cursor(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('k')));
    assert_eq!(state.remove_queue().unwrap().cursor(), 0);
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    assert!(matches!(
        update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
        [Effect::RemoveSession {
            session,
            force: false,
            force_delete_branch: false,
            ..
        }] if *session == first
    ));
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(
        state.remove_queue().unwrap().feedback().unwrap().message,
        "waiting for the current removal"
    );

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([
            (
                first,
                SessionLifecycleProjection {
                    lifecycle: SessionLifecycle::Failed,
                    failure_stage: Some(FailureStage::Delete),
                    failure_summary: Some("safe detail".to_owned()),
                },
            ),
            (second, lifecycle(SessionLifecycle::Deleting)),
        ]))),
    );
    let queue = state.remove_queue().unwrap();
    assert_eq!(queue.candidates(), &[first]);
    assert_eq!(queue.in_flight(), None);
    assert!(queue.selected().is_empty());
    assert_eq!(
        queue.feedback().unwrap().message,
        "removal paused after a session failed"
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), None);
    assert_eq!(state.remove_queue(), None);

    state.overlay = Some(Overlay::RemoveSessions);
    assert!(update_remove_queue(&mut state, &AppKey::Down).is_empty());
    assert_eq!(state.overlay(), None);
}

#[test]
fn remove_selector_covers_empty_toggle_notice_and_deleting_refresh_paths() {
    let (workspace, first, second) = ids();

    let mut empty = AppState::home(workspace, Vec::new());
    let _ = update(&mut empty, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut empty,
        AppEvent::Key(AppKey::SubmitOverview("session remove -s".to_owned())),
    );
    for key in [
        AppKey::Up,
        AppKey::Down,
        AppKey::Char('k'),
        AppKey::Char('j'),
        AppKey::Char(' '),
        AppKey::Home,
    ] {
        assert!(update(&mut empty, AppEvent::Key(key)).is_empty());
    }
    assert!(update(&mut empty, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(
        empty.remove_queue().unwrap().feedback().unwrap().message,
        "no sessions can be removed"
    );

    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("session remove -s".to_owned())),
    );
    assert_eq!(state.remove_queue().unwrap().cursor(), 0);
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.remove_queue().unwrap().cursor(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.remove_queue().unwrap().cursor(), 0);
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    assert!(state.remove_queue().unwrap().selected().is_empty());
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([
            (first, lifecycle(SessionLifecycle::Deleting)),
            (second, lifecycle(SessionLifecycle::Available)),
        ]))),
    );
    let queue = state.remove_queue().unwrap();
    assert_eq!(queue.candidates(), &[first, second]);
    assert_eq!(queue.in_flight(), Some(first));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Notice(Notice::new("remove refused"))),
    );
    let queue = state.remove_queue().unwrap();
    assert_eq!(queue.in_flight(), None);
    assert!(queue.selected().is_empty());
    assert_eq!(queue.feedback().unwrap().message, "remove refused");

    state.remove_queue = None;
    assert!(begin_next_remove(&mut state, false).is_empty());
}

#[test]
fn named_remove_rejects_missing_and_non_removable_sessions() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionNames(vec!["kept".to_owned()])),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            lifecycle(SessionLifecycle::Deleting),
        )]))),
    );

    for (command, message) in [
        ("session remove missing", "session was not found"),
        ("session remove kept", "session cannot be removed"),
    ] {
        state.overlay = Some(Overlay::Overview);
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitOverview(command.to_owned()))
            )
            .is_empty()
        );
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some(message)
        );
    }
}

#[test]
fn a_pending_pr_request_is_forgotten_when_its_session_leaves_the_workspace() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.pr_request(), Some(Target::Session(session)));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Sessions(Vec::new())),
    );
    assert_eq!(state.pr_request(), None);
}

#[test]
fn managed_navigation_defensive_boundaries_never_create_a_root_target() {
    let (workspace, session, dropped) = ids();

    // A viewport with only one content line cannot fit a two-line session
    // row. Hit-testing follows the renderer and stops before that row.
    let narrow = sized_home(workspace, vec![session], 100, 4);
    assert_eq!(narrow.sidebar_selection_at(5, 2), None);

    // A synthetic stale root cursor is repaired to the first managed row.
    let mut state = AppState::home(workspace, vec![session]);
    state.selected = Selection::Target(Target::Root(workspace));
    state.reconcile_sessions(&[]);
    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(session))
    );

    // Losing the active session while its launcher is open returns to
    // Switch and clears the stale launcher state.
    state.active = Some(dropped);
    state.route = Route::Home(HomeMode::Closeup);
    state.overlay = Some(Overlay::Closeup);
    state.closeup_action_forced = true;
    state.reconcile_sessions(&[dropped]);
    assert_eq!(state.active(), Some(session));
    state.active = None;
    state.reconcile_sessions(&[session]);
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
    assert_eq!(state.overlay(), None);
    assert!(!state.closeup_action_forced);

    // Even an internally stale Closeup cannot reopen active-target actions or
    // overlays. Ctrl-A returns to Switch, where Preview deliberately follows
    // the still-valid sidebar cursor even though there is no active target.
    state.route = Route::Home(HomeMode::Closeup);
    assert!(update_management_key(&mut state, AppKey::CtrlA).is_empty());
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
    for key in [AppKey::OpenNotes, AppKey::OpenPrs] {
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert_eq!(state.overlay(), None);
    }
    let effects = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
    let request_id = state.preview_overlay().unwrap().request_id();
    assert_eq!(
        effects,
        vec![Effect::LoadPreview {
            target: Target::Session(session),
            request_id,
            path: None,
            filter: PreviewFileFilter::All,
        }]
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    for selection in [
        Selection::Target(Target::Root(workspace)),
        Selection::Target(Target::Session(dropped)),
    ] {
        state.selected = selection;
        assert!(activate_selected(&mut state).is_empty());
        assert_eq!(state.active(), None);
    }
}

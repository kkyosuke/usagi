//! new の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn new_validation_reports_every_required_clone_and_existing_field() {
    let cases = [
        (
            NewMode::Clone,
            NewForm::default(),
            NewValidationError::RepositoryRequired,
        ),
        (
            NewMode::Clone,
            NewForm {
                repository: "repo".to_owned(),
                ..NewForm::default()
            },
            NewValidationError::LocationRequired,
        ),
        (
            NewMode::Clone,
            NewForm {
                repository: "repo".to_owned(),
                location: "/work".to_owned(),
                ..NewForm::default()
            },
            NewValidationError::DirectoryRequired,
        ),
        (
            NewMode::Existing,
            NewForm::default(),
            NewValidationError::PathRequired,
        ),
        (
            NewMode::Existing,
            NewForm {
                path: "/work/existing".to_owned(),
                ..NewForm::default()
            },
            NewValidationError::NameRequired,
        ),
    ];
    for (mode, form, expected) in cases {
        assert_eq!(validate_new_form(mode, &form), Err(expected));
        assert!(!expected.message().is_empty());
        assert!(!format!("{expected:?}").is_empty());
    }
}

#[test]
fn agent_launch_failure_opens_a_dismissible_error_dialog() {
    let (workspace, session, _) = ids();
    for dismiss in [AppKey::Escape, AppKey::Enter, AppKey::CtrlC] {
        let mut state = AppState::home(workspace, vec![session]);
        let effects = update(
            &mut state,
            AppEvent::AgentLaunchFailed(Notice::new("agent process could not be started")),
        );

        assert!(effects.is_empty());
        assert_eq!(state.overlay(), Some(Overlay::AgentLaunchError));
        assert_eq!(
            state
                .agent_launch_error()
                .map(|notice| notice.message.as_str()),
            Some("agent process could not be started")
        );
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("agent process could not be started")
        );

        assert!(update(&mut state, AppEvent::Key(dismiss)).is_empty());
        assert_eq!(state.overlay(), None);
        assert!(state.agent_launch_error().is_none());
    }
}

#[test]
fn overview_daemon_opens_the_status_surface_without_an_effect() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);

    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("daemon".into()))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Daemon));
    assert!(state.notice().is_none());
    assert!(update(&mut state, AppEvent::Key(AppKey::Escape)).is_empty());
    assert_eq!(state.overlay(), None);

    state.overlay = Some(Overlay::Overview);
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("daemon extra".into()))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(
        state
            .notice()
            .is_some_and(|notice| notice.message.as_str().contains("takes no arguments"))
    );
}

#[test]
fn overview_env_opens_the_editor_and_rejects_an_unknown_scope() {
    let (workspace, _, _) = ids();

    // The `env` command (Prompt-mode raw text or Action-mode candidate) opens
    // this workspace's editor and requests a read.
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("env".to_owned())),
    );
    assert_eq!(
        effects,
        vec![Effect::LoadEnvironment {
            scope: EnvScope::Workspace,
        }]
    );
    assert_eq!(state.overlay(), Some(Overlay::Environment));

    // Whitespace-only arguments are still treated as no arguments.
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("env   ".to_owned())),
    );
    assert_eq!(
        effects,
        vec![Effect::LoadEnvironment {
            scope: EnvScope::Workspace,
        }]
    );

    // Each scope can be named explicitly.
    for (input, scope) in [
        ("env workspace", EnvScope::Workspace),
        ("env global", EnvScope::Global),
    ] {
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let effects = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview(input.to_owned())),
        );
        assert_eq!(effects, vec![Effect::LoadEnvironment { scope }]);
        assert_eq!(state.environment_editor().unwrap().scope(), scope);
    }

    // An unknown scope is rejected safely: the editor never opens, the
    // Overview stays up, and a safe notice explains the usage.
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("env extra".to_owned())),
    );
    assert!(effects.is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.environment_editor().is_none());
}

#[test]
fn environment_keys_are_inert_without_an_open_editor() {
    let (workspace, session, _) = ids();
    let environment_keys = || [AppKey::SaveEnvironment];

    // With no overlay at all.
    let mut state = AppState::home(workspace, Vec::new());
    for key in environment_keys() {
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert!(state.environment_editor().is_none());
    }

    // And while a different editor owns input: the notes overlay keeps its
    // own draft, and no environment editor appears behind it.
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenNotes));
    for key in environment_keys() {
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert!(state.environment_editor().is_none());
    }
    assert!(state.note_editor().is_some());
}

#[test]
fn decision_snapshots_auto_open_only_for_new_pending_rows_without_stealing_an_overlay() {
    let workspace = WorkspaceId::new();
    let first = pending_decision(workspace);
    let second = pending_decision(workspace);
    let mut state = AppState::home(workspace, Vec::new());

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions: vec![first.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Decisions));
    assert!(state.decision_overlay().is_some());
    assert_eq!(
        state
            .decision_overlay()
            .and_then(DecisionOverlayState::editor)
            .map(|editor| editor.decision().decision_id),
        Some(first.decision_id)
    );
    assert_eq!(state.unread_decision_ids().len(), 1);

    // The response view closes only after the daemon confirms its durable
    // resolve; this is the request -> modal -> answer -> close path.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DecisionResolved {
            workspace,
            decision_id: first.decision_id,
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.decision_overlay().is_none());

    // Dismissal changes only UI state. A duplicate/resync snapshot must not
    // steal focus again, while a genuinely new pending row may notify.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), None);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions: vec![],
        }),
    );
    assert_eq!(state.overlay(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions: vec![first, second],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert_eq!(state.decisions().len(), 2);
}

/// Rows the preview finder currently offers.
fn visible(state: &AppState) -> usize {
    state.preview_overlay().unwrap().visible_candidates().len()
}

#[test]
fn preview_overlay_finds_opens_scrolls_and_returns_to_the_file_list() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);

    // `v` opens the preview overlay for the active target and requests it.
    let effects = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
    assert_preview_load(&state, &effects, target, None, PreviewFileFilter::All);
    assert_eq!(state.overlay(), Some(Overlay::Preview));
    assert!(state.preview_overlay().unwrap().is_loading());
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());

    // A file list for another target is ignored; unsafe backend paths are
    // rejected when the matching result lands.
    let event = preview_loaded(&state, Target::Root(workspace), None, &["stale"], &[]);
    let _ = update(&mut state, event);
    assert_eq!(visible(&state), 0);
    let event = preview_loaded(
        &state,
        target,
        None,
        &["src/lib.rs", "README.md", "src/runtime.rs", "bad\npath"],
        &[],
    );
    let _ = update(&mut state, event);
    assert!(!state.preview_overlay().unwrap().is_loading());
    assert_eq!(visible(&state), 3);

    // Finder navigation saturates, and filtering resets selection. A query
    // with several matches also exercises fuzzy rank ordering.
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('s')));
    assert_eq!(visible(&state), 2);
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.preview_overlay().unwrap().selected(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.preview_overlay().unwrap().selected(), 0);

    // Paste drops terminal controls before the query reaches presentation.
    let _ = update(&mut state, AppEvent::Key(AppKey::Paste("s\u{1b}rm".into())));
    let overlay = state.preview_overlay().unwrap();
    assert_eq!(overlay.filter(), "srm");
    assert_eq!(overlay.selected(), 0);
    assert_eq!(overlay.selected_file(), Some("src/runtime.rs"));

    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_preview_load(
        &state,
        &effects,
        target,
        Some("src/runtime.rs"),
        PreviewFileFilter::All,
    );
    assert_eq!(
        state.preview_overlay().unwrap().path(),
        Some("src/runtime.rs")
    );
    assert!(state.preview_overlay().unwrap().is_loading());

    // A late completion for another file cannot replace the requested one.
    let event = preview_loaded(&state, target, Some("src/lib.rs"), &[], &["stale"]);
    let _ = update(&mut state, event);
    assert!(state.preview_overlay().unwrap().lines().is_empty());
    let event = preview_loaded(
        &state,
        target,
        Some("src/runtime.rs"),
        &[],
        &["# Title", "\u{1b}[31mred\ttext"],
    );
    let _ = update(&mut state, event);
    assert_eq!(
        state.preview_overlay().unwrap().lines(),
        &["# Title", "�[31mred text"]
    );
    assert!(update(&mut state, AppEvent::Key(AppKey::Home)).is_empty());

    // Down scrolls; Up saturates at the top.
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.preview_overlay().unwrap().scroll(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.preview_overlay().unwrap().scroll(), 0);

    // A safe read error surfaces on the open overlay.
    let request_id = state.preview_overlay().unwrap().request_id();
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PreviewError {
            target,
            request_id,
            path: Some("src/runtime.rs".into()),
            filter: PreviewFileFilter::All,
            error: safe_error("no preview"),
        }),
    );
    assert_eq!(
        state
            .preview_overlay()
            .unwrap()
            .error()
            .map(|error| error.message.as_str()),
        Some("no preview")
    );

    // The first Esc returns to the cached finder; the second closes it.
    escape_preview(&mut state);
    assert_eq!(state.overlay(), Some(Overlay::Preview));
    assert_eq!(state.preview_overlay().unwrap().path(), None);
    assert_eq!(
        state.preview_overlay().unwrap().selected_file(),
        Some("src/runtime.rs")
    );
    escape_preview(&mut state);
    assert_eq!(state.overlay(), None);
    assert!(state.preview_overlay().is_none());
}

#[test]
fn opening_one_overlay_discards_the_other_state() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    // Open PRs, dismiss, then open preview: the PR state must not linger.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert!(state.pr_request().is_some());
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
    assert_eq!(state.overlay(), Some(Overlay::Preview));
    assert!(state.pr_overlay().is_none());
    // The preview supersedes the request, so its late snapshot cannot open a
    // modal the user is no longer asking for.
    assert_eq!(state.pr_request(), None);
    assert!(state.preview_overlay().is_some());
    // And the reverse: opening PRs discards the preview state.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), None);
    assert!(state.pr_request().is_some());
    assert!(state.preview_overlay().is_none());
}

#[test]
fn typed_overlay_accessors_keep_their_target_and_editor_state() {
    let (workspace, session, _) = ids();
    let root = Target::Root(workspace);

    let note = NoteEditor::loading(root);
    assert_eq!(note.target(), root);

    let mut decision = pending_decision(workspace);
    decision.allow_freeform = true;
    let editor = DecisionEditor::new(decision);
    assert_eq!(editor.selected_option(), 0);
    assert_eq!(editor.freeform(), "");
    assert!(editor.error().is_none());
    let overlay = DecisionOverlayState {
        show_answers: false,
        selected: 0,
        editor: Some(editor),
        requested: None,
        error: None,
        exact: None,
    };
    assert_eq!(overlay.selected(), 0);

    let prs = PrOverlay::showing(root, Vec::new(), None);
    assert_eq!(prs.target(), root);
    let preview = PreviewOverlay::loading(root, Vec::new());
    assert_eq!(preview.target(), root);
    let environment = EnvironmentEditor::loading(EnvScope::Global);
    assert_eq!(environment.scope(), EnvScope::Global);

    assert_eq!(PendingToken::from_raw(7).get(), 7);
    assert_eq!(root.session_id(), None);
    assert_eq!(Target::Session(session).session_id(), Some(session));
}

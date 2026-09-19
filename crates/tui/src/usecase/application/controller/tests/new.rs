//! new の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;

#[test]
fn new_clone_validates_dispatches_progress_and_attaches_home_on_success() {
    let (workspace, session, _) = ids();
    let mut state = NewState::new(NewMode::Clone, clone_form());
    let mut backend = FakeNewBackend::default();
    let effects = update_new(&mut state, NewEvent::Submit);
    assert_eq!(state.pending(), Some(PendingToken(1)));
    assert_eq!(
        state.progress().map(SafeMessage::as_str),
        Some("Cloning repository…")
    );
    assert_eq!(
        effects,
        vec![Effect::CloneProject {
            repository: "https://example.com/acme/app.git".to_owned(),
            destination: PathBuf::from("/work/app"),
            branch: Some("main".to_owned()),
            token: PendingToken(1),
        }]
    );
    backend.push_event(NewEvent::Result {
        token: PendingToken(1),
        result: Ok(HomeSnapshot::new(workspace, vec![session])),
    });
    run_new_fake_cycle(&mut state, &mut backend, effects);
    assert_eq!(backend.effects().len(), 1);
    assert_eq!(state.pending(), None);
    assert_eq!(state.progress(), None);
    assert!(matches!(
        state.route(),
        NewRoute::Home(home) if home.workspace() == workspace && home.sessions() == [session]
    ));
}

#[test]
fn new_submit_while_pending_ignores_the_duplicate_operation() {
    let mut state = NewState::new(NewMode::Clone, clone_form());
    let first = update_new(&mut state, NewEvent::Submit);
    assert_eq!(first.len(), 1);
    assert_eq!(state.pending(), Some(PendingToken(1)));

    // A second Submit before the backend completes is a no-op: it produces
    // no new effect and does not advance the pending token, so a fast double
    // Enter cannot start two clones.
    let second = update_new(&mut state, NewEvent::Submit);
    assert!(second.is_empty());
    assert_eq!(state.pending(), Some(PendingToken(1)));

    // Retry is guarded the same way while an operation is in flight.
    assert!(update_new(&mut state, NewEvent::Retry).is_empty());
    assert_eq!(state.pending(), Some(PendingToken(1)));
}

#[test]
fn new_existing_failure_retains_form_and_retry_reuses_the_request() {
    let mut state = NewState::new(NewMode::Existing, existing_form());
    let effects = update_new(&mut state, NewEvent::Submit);
    let expected = Effect::RegisterWorkspace {
        path: PathBuf::from("/work/existing"),
        name: "existing".to_owned(),
        token: PendingToken(1),
    };
    assert_eq!(effects, vec![expected]);
    let _ = update_new(
        &mut state,
        NewEvent::Result {
            token: PendingToken(1),
            result: Err(Notice::new("directory is not a project")),
        },
    );
    assert!(matches!(state.route(), NewRoute::Form));
    assert_eq!(state.form(), &existing_form());
    assert_eq!(
        state.error().map(|notice| notice.message.as_str()),
        Some("directory is not a project")
    );
    assert_eq!(state.progress(), None);

    assert_eq!(
        update_new(&mut state, NewEvent::Retry),
        vec![Effect::RegisterWorkspace {
            path: PathBuf::from("/work/existing"),
            name: "existing".to_owned(),
            token: PendingToken(2),
        }]
    );
    assert_eq!(
        state.progress().map(SafeMessage::as_str),
        Some("Registering workspace…")
    );
}

#[test]
fn new_validation_and_late_completion_keep_the_form_route() {
    let mut invalid = NewState::new(NewMode::Clone, NewForm::default());
    assert!(update_new(&mut invalid, NewEvent::Submit).is_empty());
    assert_eq!(
        invalid.error().map(|notice| notice.message.as_str()),
        Some("repository URL is required")
    );

    let mut state = NewState::new(NewMode::Existing, existing_form());
    let _ = update_new(&mut state, NewEvent::Submit);
    let _ = update_new(
        &mut state,
        NewEvent::Result {
            token: PendingToken(99),
            result: Err(Notice::new("late failure")),
        },
    );
    assert_eq!(state.pending(), Some(PendingToken(1)));
    assert!(matches!(state.route(), NewRoute::Form));
    assert_eq!(state.error(), None);
}

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
fn goal_driven_new_requires_one_goal_and_emits_only_the_goal_launch() {
    let workspace = WorkspaceId::new();
    let mut state = sized_home(workspace, Vec::new(), 100, 30);
    state.set_agent_models(
        AvailableModels::new([DefaultModel::OpenAi]),
        DefaultModel::OpenAi,
    );
    state.set_work_mode(WorkMode::GoalDriven);

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    assert_eq!(state.director_goal(), "");
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    for character in "目的を実装する".chars() {
        let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
    }
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    let [
        Effect::LaunchGoal {
            workspace: actual,
            operation_id,
            profile: Some(profile),
            goal,
        },
    ] = effects.as_slice()
    else {
        panic!("goal confirmation must emit one launch: {effects:?}");
    };
    assert_eq!(*actual, workspace);
    assert_eq!(profile.as_str(), "codex");
    assert_eq!(goal, "目的を実装する");
    assert_eq!(state.director_goal(), "");
    assert!(state.director_launching().is_some());
    let run = SupervisorRunId::new();
    let _ = update(
        &mut state,
        AppEvent::DirectorLaunchFinished {
            operation: *operation_id,
            supervisor_run_id: Some(run),
            succeeded: true,
        },
    );
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));
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
fn entry_open_single_preserves_the_selected_identity_into_home() {
    let first = WorkspaceId::new();
    let chosen = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = EntryState::new(
        vec![
            EntryWorkspace::new(first, "renamed later"),
            EntryWorkspace::new(chosen, "selected"),
        ],
        Vec::new(),
    );

    assert!(update_entry(&mut state, EntryEvent::ShowOpen).is_empty());
    assert_eq!(
        update_entry(&mut state, EntryEvent::OpenSingle(chosen)),
        vec![Effect::AttachWorkspace { workspace: chosen }]
    );
    let _ = update_entry(
        &mut state,
        EntryEvent::AttachResult {
            workspace: chosen,
            result: Ok(HomeSnapshot::new(chosen, vec![session])),
        },
    );

    let EntryRoute::Home(home) = state.route() else {
        panic!("selected workspace should enter Home");
    };
    assert_eq!(home.workspace(), chosen);
    assert_eq!(home.sessions(), &[session]);
    assert_eq!(home.selected(), Selection::Target(Target::Session(session)));
}

#[test]
fn entry_recent_uses_its_identity_and_ignores_stale_completion() {
    let recent = WorkspaceId::new();
    let delayed_workspace = WorkspaceId::new();
    let mut state = EntryState::new(Vec::new(), vec![recent]);

    assert_eq!(
        update_entry(&mut state, EntryEvent::OpenRecent(recent)),
        vec![Effect::AttachWorkspace { workspace: recent }]
    );
    let _ = update_entry(
        &mut state,
        EntryEvent::AttachResult {
            workspace: delayed_workspace,
            result: Ok(HomeSnapshot::new(delayed_workspace, Vec::new())),
        },
    );
    assert_eq!(state.route(), &EntryRoute::Welcome);
    assert_eq!(state.opening(), Some(recent));
    assert!(state.error().is_none());

    let _ = update_entry(
        &mut state,
        EntryEvent::AttachResult {
            workspace: recent,
            result: Ok(HomeSnapshot::new(recent, Vec::new())),
        },
    );
    assert!(matches!(state.route(), EntryRoute::Home(home) if home.workspace() == recent));
}

#[test]
fn fake_entry_backend_replays_error_then_retry_without_opening_another_workspace() {
    let requested = WorkspaceId::new();
    let other = WorkspaceId::new();
    let mut state = EntryState::new(Vec::new(), vec![requested]);
    let mut backend = FakeEntryBackend::default();
    backend.push_event(EntryEvent::AttachResult {
        workspace: other,
        result: Ok(HomeSnapshot::new(other, Vec::new())),
    });
    backend.push_event(EntryEvent::AttachResult {
        workspace: requested,
        result: Err(Notice::new("temporary attach failure")),
    });

    let effects = update_entry(&mut state, EntryEvent::OpenRecent(requested));
    run_entry_fake_cycle(&mut state, &mut backend, effects);
    assert_eq!(state.route(), &EntryRoute::Welcome);
    assert_eq!(
        state.error().map(|notice| notice.message.as_str()),
        Some("temporary attach failure")
    );

    let retry = update_entry(&mut state, EntryEvent::Retry);
    run_entry_fake_cycle(&mut state, &mut backend, retry);
    assert_eq!(
        backend.effects(),
        &[
            Effect::AttachWorkspace {
                workspace: requested
            },
            Effect::AttachWorkspace {
                workspace: requested
            }
        ]
    );
    assert_eq!(state.opening(), Some(requested));
    assert_eq!(state.route(), &EntryRoute::Welcome);
}

#[test]
fn entry_empty_open_and_unknown_recent_are_noops() {
    let unknown = WorkspaceId::new();
    let mut state = EntryState::new(Vec::new(), Vec::new());
    let _ = update_entry(&mut state, EntryEvent::ShowOpen);

    assert!(update_entry(&mut state, EntryEvent::OpenSingle(unknown)).is_empty());
    assert!(update_entry(&mut state, EntryEvent::Back).is_empty());
    assert_eq!(state.route(), &EntryRoute::Welcome);
    assert!(update_entry(&mut state, EntryEvent::OpenRecent(unknown)).is_empty());
}

#[test]
fn entry_open_error_stays_on_its_screen_and_retries_the_same_identity() {
    let workspace = WorkspaceId::new();
    let mut state = EntryState::new(
        vec![EntryWorkspace::new(workspace, "broken registration")],
        Vec::new(),
    );
    let _ = update_entry(&mut state, EntryEvent::ShowOpen);
    let _ = update_entry(&mut state, EntryEvent::OpenSingle(workspace));
    let _ = update_entry(
        &mut state,
        EntryEvent::AttachResult {
            workspace,
            result: Err(Notice::new("workspace is unavailable")),
        },
    );

    assert_eq!(state.route(), &EntryRoute::Open);
    assert_eq!(
        state.error().map(|notice| notice.message.as_str()),
        Some("workspace is unavailable")
    );
    assert_eq!(
        update_entry(&mut state, EntryEvent::Retry),
        vec![Effect::AttachWorkspace { workspace }]
    );
    assert_eq!(state.opening(), Some(workspace));
}

#[test]
fn entry_rejects_a_snapshot_for_another_workspace_and_allows_retry() {
    let requested = WorkspaceId::new();
    let returned = WorkspaceId::new();
    let mut state = EntryState::new(Vec::new(), vec![requested]);
    let _ = update_entry(&mut state, EntryEvent::OpenRecent(requested));
    let _ = update_entry(
        &mut state,
        EntryEvent::AttachResult {
            workspace: requested,
            result: Ok(HomeSnapshot::new(returned, Vec::new())),
        },
    );

    assert_eq!(state.route(), &EntryRoute::Welcome);
    assert_eq!(
        state.error().map(|notice| notice.message.as_str()),
        Some("workspace changed while opening; retry")
    );
    assert_eq!(
        update_entry(&mut state, EntryEvent::Retry),
        vec![Effect::AttachWorkspace {
            workspace: requested
        }]
    );
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
fn coverage_contract_exposes_every_typed_overlay_and_entry_accessor() {
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
        selected: 0,
        editor: Some(editor),
    };
    assert_eq!(overlay.selected(), 0);

    let prs = PrOverlay::showing(root, Vec::new(), None);
    assert_eq!(prs.target(), root);
    let preview = PreviewOverlay::loading(root);
    assert_eq!(preview.target(), root);
    let environment = EnvironmentEditor::loading(EnvScope::Global);
    assert_eq!(environment.scope(), EnvScope::Global);

    assert_eq!(PendingToken::from_raw(7).get(), 7);
    let entry_workspace = EntryWorkspace::new(workspace, "repo");
    let entry = EntryState::new(vec![entry_workspace.clone()], vec![workspace]);
    assert_eq!(entry.workspaces(), std::slice::from_ref(&entry_workspace));
    assert_eq!(entry.recents(), &[workspace]);

    let new = NewState::new(NewMode::Existing, existing_form());
    assert_eq!(new.mode(), NewMode::Existing);
    assert_eq!(root.session_id(), None);
    assert_eq!(Target::Session(session).session_id(), Some(session));
}

#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

mod closeup;
mod director;
mod garden;
mod new;
mod pointer;
mod pr;
mod session;
mod workflow;

use super::*;
use crate::usecase::application::environment_source::parse_environment_source;
use std::collections::VecDeque;

#[test]
fn a_background_read_never_swallows_the_person_s_submission() {
    use crate::usecase::application::workflow::WorkflowJob;
    use usagi_core::domain::workflow::{WorkflowCommand, WorkflowSnapshot};
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let _ = submit_closeup_workflow(&mut state, session, "");
    // Opening the tab leaves a read in flight. The pane re-reads on a steady
    // cadence, so a person who waits for it to clear waits forever.
    assert!(state.workflow_panel(session).unwrap().loading);
    assert_eq!(
        state.workflow_panel(session).unwrap().freshness,
        crate::usecase::application::workflow::WorkflowFreshness::Pending
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::Paste("Build login".into()),
        },
    );
    let effects = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let [Effect::Workflow(start)] = effects.as_slice() else {
        panic!("the submission is dispatched, got {effects:?}");
    };
    assert!(
        matches!(&start.control, Some((_, WorkflowCommand::Start { goal, .. })) if goal == "Build login")
    );

    // The read it overlapped lands without disturbing the submission.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: WorkflowJob {
                workspace,
                session,
                control: None,
            },
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: None,
                pending_start: None,
                finished: Vec::new(),
            })),
        }),
    );
    let panel = state.workflow_panel(session).unwrap();
    assert!(!panel.loading);
    assert_eq!(
        panel.freshness,
        crate::usecase::application::workflow::WorkflowFreshness::Observed
    );
    assert!(panel.submitting);
    assert!(panel.pending.is_some());
}

/// Fake entry backend for Welcome / Open attach scenarios. It has no IO: tests
/// inspect dispatched effects and enqueue typed completions in deterministic order.
#[derive(Debug, Default)]
#[cfg(test)]
struct FakeEntryBackend {
    effects: Vec<Effect>,
    events: VecDeque<EntryEvent>,
}

#[cfg(test)]
impl FakeEntryBackend {
    /// Queue one attach completion (including a deliberately stale one).
    fn push_event(&mut self, event: EntryEvent) {
        self.events.push_back(event);
    }

    /// Effects dispatched by the entry reducer.
    #[must_use]
    fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

/// Dispatch entry effects and replay queued fake-backend completions.
#[cfg(test)]
fn run_entry_fake_cycle(
    state: &mut EntryState,
    backend: &mut FakeEntryBackend,
    effects: Vec<Effect>,
) {
    backend.effects.extend(effects);
    while let Some(event) = backend.events.pop_front() {
        let _ = update_entry(state, event);
    }
}

/// Backend seam for New. Implementations route clone through git then project
/// registration, and Existing directly through project/registry registration.
#[cfg(test)]
trait NewProjectPort {
    /// Dispatch one New operation.
    fn dispatch(&mut self, effect: Effect);
    /// Return the next completion, if any.
    fn next_event(&mut self) -> Option<NewEvent>;
}

/// IO-free backend used by New reducer scenarios.
#[derive(Debug, Default)]
#[cfg(test)]
struct FakeNewBackend {
    effects: Vec<Effect>,
    events: VecDeque<NewEvent>,
}

#[cfg(test)]
impl FakeNewBackend {
    fn push_event(&mut self, event: NewEvent) {
        self.events.push_back(event);
    }
    #[must_use]
    fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

#[cfg(test)]
impl NewProjectPort for FakeNewBackend {
    fn dispatch(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
    fn next_event(&mut self) -> Option<NewEvent> {
        self.events.pop_front()
    }
}

/// Dispatch New effects and replay queued fake-backend completions.
#[cfg(test)]
fn run_new_fake_cycle(
    state: &mut NewState,
    backend: &mut impl NewProjectPort,
    effects: Vec<Effect>,
) {
    for effect in effects {
        backend.dispatch(effect);
    }
    while let Some(event) = backend.next_event() {
        let _ = update_new(state, event);
    }
}

/// effect を実行し、backend event を取り出す TUI-local port。
#[cfg(test)]
trait BackendPort {
    /// reducer が返した effect を 1 件 dispatch する。
    fn dispatch(&mut self, effect: Effect);
    /// 次の projection event。無ければ `None`。
    fn next_event(&mut self) -> Option<BackendEvent>;
}

/// reducer scenario 用の backend。request log と event queue のみを持ち、IO はしない。
#[derive(Debug, Default)]
#[cfg(test)]
struct FakeControllerBackend {
    effects: Vec<Effect>,
    events: VecDeque<BackendEvent>,
}

#[cfg(test)]
impl FakeControllerBackend {
    /// backend から届く event を末尾に積む。
    fn push_event(&mut self, event: BackendEvent) {
        self.events.push_back(event);
    }
    /// dispatch された effect を確認する。
    #[must_use]
    fn effects(&self) -> &[Effect] {
        &self.effects
    }
    /// effect log を取り出し、空にする。
    #[must_use]
    fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

#[cfg(test)]
impl BackendPort for FakeControllerBackend {
    fn dispatch(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.events.pop_front()
    }
}

/// event を state へ還元し、必要な外部 effect を返す。
#[cfg(test)]
fn run_fake_cycle(state: &mut AppState, backend: &mut impl BackendPort, effects: Vec<Effect>) {
    for effect in effects {
        backend.dispatch(effect);
    }
    while let Some(event) = backend.next_event() {
        let _ = update(state, AppEvent::Backend(event));
    }
}

use usagi_core::domain::id::{
    AgentRuntimeId, DaemonGeneration, TerminalId, TerminalRef, WorktreeId,
};

fn ids() -> (WorkspaceId, SessionId, SessionId) {
    (WorkspaceId::new(), SessionId::new(), SessionId::new())
}

fn lifecycle(value: SessionLifecycle) -> SessionLifecycleProjection {
    SessionLifecycleProjection {
        lifecycle: value,
        failure_stage: None,
        failure_summary: None,
    }
}

fn sized_home(
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    width: u16,
    height: u16,
) -> AppState {
    let mut state = AppState::home(workspace, sessions);
    let _ = update(&mut state, AppEvent::Resize { width, height });
    state
}

fn click_at(state: &mut AppState, column: u16, row: u16, at_ms: u64) -> Selection {
    let _ = update(
        state,
        AppEvent::Pointer {
            column,
            row,
            at: std::time::Duration::from_millis(at_ms),
        },
    );
    state.selected()
}

#[test]
fn role_projection_does_not_change_lifecycle_capabilities() {
    let (workspace, first, _) = ids();
    let mut state = AppState::home(workspace, vec![first]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            first,
            lifecycle(SessionLifecycle::Failed),
        )]))),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionRoles(BTreeMap::from([(
            first,
            SessionRoleProjection {
                role_id: Some(RoleId::new("reviewer").unwrap()),
                role_summary: None,
                parent_session_id: None,
                agent_status: None,
            },
        )]))),
    );
    assert_eq!(
        state.session_roles()[&first]
            .role_id
            .as_ref()
            .unwrap()
            .as_str(),
        "reviewer"
    );
    assert!(!state.session_can_use(first));
}

#[test]
fn management_classifier_rejects_extra_modifiers_for_ctrl_x() {
    for modifiers in [
        crate::usecase::terminal_input::Modifiers {
            control: true,
            shift: true,
            alt: true,
            ..crate::usecase::terminal_input::Modifiers::default()
        },
        crate::usecase::terminal_input::Modifiers {
            control: true,
            shift: true,
            super_: true,
            ..crate::usecase::terminal_input::Modifiers::default()
        },
        crate::usecase::terminal_input::Modifiers {
            control: true,
            shift: true,
            hyper: true,
            ..crate::usecase::terminal_input::Modifiers::default()
        },
        crate::usecase::terminal_input::Modifiers {
            control: true,
            shift: true,
            meta: true,
            ..crate::usecase::terminal_input::Modifiers::default()
        },
    ] {
        assert_eq!(
            classify_management_input(LiveInput::Key(
                crate::usecase::terminal_input::KeyEvent::new(
                    KeyCode::Char('X'),
                    modifiers,
                    KeyEventKind::Press,
                )
            )),
            None
        );
    }
}

#[test]
fn management_classifier_keeps_navigation_ctrl_a_and_ignores_caret_only_keys() {
    use crate::usecase::terminal_input::Modifiers;
    let key = |code, modifiers| {
        LiveInput::Key(crate::usecase::terminal_input::KeyEvent::new(
            code,
            modifiers,
            KeyEventKind::Press,
        ))
    };
    let plain = Modifiers::default;
    let shift = || Modifiers {
        shift: true,
        ..plain()
    };
    // Home and Ctrl-A remain the `+ new session` action in navigation; this
    // reducer has no byte caret, so the caret-only edits (End / Ctrl-E,
    // Delete, and Shift+motion selection) are inert here and never
    // mis-route into the string-only create form.
    assert_eq!(
        classify_management_input(key(KeyCode::Home, plain())),
        Some(AppKey::CtrlA)
    );
    assert_eq!(classify_management_input(key(KeyCode::End, plain())), None);
    assert_eq!(
        classify_management_input(key(
            KeyCode::Char('e'),
            Modifiers {
                control: true,
                ..plain()
            }
        )),
        None
    );
    assert_eq!(
        classify_management_input(key(KeyCode::Delete, plain())),
        None
    );
    assert_eq!(
        classify_management_input(key(KeyCode::Home, shift())),
        Some(AppKey::CtrlA),
        "Shift+Home still resolves to the navigation action, not a selection"
    );
}

#[test]
fn management_classifier_covers_non_key_release_navigation_and_text() {
    use crate::usecase::terminal_input::{KeyEvent, Modifiers};
    assert_eq!(classify_management_input(LiveInput::Paste(vec![])), None);
    assert_eq!(
        classify_management_input(LiveInput::Key(KeyEvent::new(
            KeyCode::Enter,
            Modifiers::default(),
            KeyEventKind::Release,
        ))),
        None
    );
    for (code, expected) in [
        (KeyCode::Enter, AppKey::Enter),
        (KeyCode::Tab, AppKey::Tab),
        (KeyCode::Backspace, AppKey::Backspace),
        (KeyCode::Escape, AppKey::Escape),
        (KeyCode::Up, AppKey::Up),
        (KeyCode::Down, AppKey::Down),
        (KeyCode::Left, AppKey::Left),
        (KeyCode::Right, AppKey::Right),
        (KeyCode::Char('x'), AppKey::Char('x')),
    ] {
        assert_eq!(
            classify_management_input(LiveInput::Key(KeyEvent::new(
                code,
                Modifiers::default(),
                KeyEventKind::Press,
            ))),
            Some(expected)
        );
    }
}

fn clone_form() -> NewForm {
    NewForm {
        repository: " https://example.com/acme/app.git ".to_owned(),
        location: " /work ".to_owned(),
        directory: " app ".to_owned(),
        branch: " main ".to_owned(),
        ..NewForm::default()
    }
}

fn existing_form() -> NewForm {
    NewForm {
        path: " /work/existing ".to_owned(),
        name: " existing ".to_owned(),
        ..NewForm::default()
    }
}

fn runtime(workspace: WorkspaceId, session: SessionId) -> AgentRuntimeRef {
    AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        },
        Some(session),
    )
    .unwrap()
}

#[test]
fn tick_advances_only_the_mascot_animation_frame() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.mascot_tick(), 0);
    let _ = update(&mut state, AppEvent::Tick);
    assert_eq!(state.mascot_tick(), 1);
}

#[test]
fn table_driven_mode_and_overlay_scenarios() {
    struct Case {
        name: &'static str,
        events: Vec<AppEvent>,
        route: Route,
        overlay: Option<Overlay>,
    }
    let (workspace, first, _) = ids();
    let cases = [
        Case {
            name: "switch escape is no-op",
            events: vec![AppEvent::Key(AppKey::Escape)],
            route: Route::Home(HomeMode::Switch),
            overlay: None,
        },
        Case {
            name: "overview returns to switch origin",
            events: vec![
                AppEvent::Key(AppKey::OpenOverview),
                AppEvent::Key(AppKey::Escape),
            ],
            route: Route::Home(HomeMode::Switch),
            overlay: None,
        },
        Case {
            name: "closeup overlay escape returns to closeup",
            events: vec![
                AppEvent::LivePaneAvailability(true),
                AppEvent::Key(AppKey::Enter),
                AppEvent::Key(AppKey::OpenCloseupOverlay),
                AppEvent::Key(AppKey::Escape),
            ],
            route: Route::Home(HomeMode::Closeup),
            overlay: None,
        },
        Case {
            name: "closeup escape is no-op",
            events: vec![
                AppEvent::LivePaneAvailability(true),
                AppEvent::Key(AppKey::Enter),
                AppEvent::Key(AppKey::Escape),
            ],
            route: Route::Home(HomeMode::Closeup),
            overlay: None,
        },
    ];
    for case in cases {
        let mut state = AppState::home(workspace, vec![first]);
        for event in case.events {
            let _ = update(&mut state, event);
        }
        assert_eq!(state.route(), case.route, "{}", case.name);
        assert_eq!(state.overlay(), case.overlay, "{}", case.name);
    }
}

#[test]
fn goal_composer_edits_utf8_within_the_daemon_bound_and_discards_drafts() {
    let workspace = WorkspaceId::new();
    let mut state = sized_home(workspace, Vec::new(), 100, 30);
    state.set_work_mode(WorkMode::GoalDriven);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));

    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::Paste("x".repeat(MAX_WORK_GOAL_BYTES + 1))),
    );
    assert_eq!(state.director_goal().len(), MAX_WORK_GOAL_BYTES);
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
    assert_eq!(state.director_goal().len(), MAX_WORK_GOAL_BYTES);
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('é')));
    let _ = update(&mut state, AppEvent::Key(AppKey::Paste("é".to_owned())));
    assert_eq!(state.director_goal().len(), MAX_WORK_GOAL_BYTES - 1);

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.director_goal(), "");
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('z')));
    assert_eq!(state.director_goal(), "");

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('z')));
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlC));
    assert_eq!(state.director_goal(), "");

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('z')));
    state.set_work_mode(WorkMode::Classic);
    assert_eq!(state.director_goal(), "");
}

#[test]
fn goal_composer_enter_requires_its_provider_to_be_visible() {
    assert_eq!(
        director_goal_composer_picker_capacity(0),
        NORMALIZED_TERMINAL_ROWS - DIRECTOR_GOAL_COMPOSER_CHROME_ROWS
    );
    assert_eq!(director_goal_composer_picker_capacity(11), 0);
    assert_eq!(director_goal_composer_picker_capacity(12), 1);

    let workspace = WorkspaceId::new();
    for (height, launches) in [(11_u16, 0_usize), (12, 1)] {
        let mut state = AppState::home(workspace, Vec::new());
        state.set_agent_models(
            AvailableModels::new([DefaultModel::OpenAi]),
            DefaultModel::OpenAi,
        );
        state.set_work_mode(WorkMode::GoalDriven);
        let _ = update(&mut state, AppEvent::Resize { width: 80, height });
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::Paste("finish the PR".to_owned())),
        );
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::LaunchGoal { .. }))
                .count(),
            launches,
            "height {height}"
        );
        assert_eq!(state.director_launching().is_some(), launches == 1);
        assert_eq!(
            matches!(state.director_new(), DirectorNew::Choosing(_)),
            launches == 0
        );
    }
}

#[test]
fn management_ctrl_q_always_confirms_and_confirmation_can_cancel() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlQ)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
    // Opening the prompt focuses Quit by default, so the historical
    // `Ctrl-Q` + `Enter` still ends the process rather than leaving.
    assert_eq!(state.exit_choice(), ExitChoice::Quit);
    assert!(update(&mut state, AppEvent::Key(AppKey::Char('n'))).is_empty());
    assert_eq!(state.overlay(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenQuitConfirmation));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::Detach]
    );
}

/// #556: quitting the process and leaving for Welcome are separate answers
/// with separate letters and separate effects, so neither is reachable by
/// mistyping the other.
#[test]
fn exit_prompt_separates_leaving_from_quitting_on_every_route() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());

    // Each letter commits its own answer regardless of the focused button,
    // and every answer closes the prompt.
    for (key, choice, effects) in [
        (
            AppKey::Char('w'),
            ExitChoice::Welcome,
            vec![Effect::LeaveWorkspace],
        ),
        (
            AppKey::Char('W'),
            ExitChoice::Welcome,
            vec![Effect::LeaveWorkspace],
        ),
        (AppKey::Char('q'), ExitChoice::Quit, vec![Effect::Detach]),
        (AppKey::Char('Q'), ExitChoice::Quit, vec![Effect::Detach]),
        (AppKey::Char('y'), ExitChoice::Quit, vec![Effect::Detach]),
        (AppKey::Char('n'), ExitChoice::Stay, Vec::new()),
        (AppKey::Char('N'), ExitChoice::Stay, Vec::new()),
        (AppKey::Escape, ExitChoice::Stay, Vec::new()),
    ] {
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
        assert_eq!(state.exit_choice(), ExitChoice::Quit, "{key:?}");
        assert_eq!(
            update(&mut state, AppEvent::Key(key.clone())),
            effects,
            "{key:?}"
        );
        assert_eq!(state.exit_choice(), choice, "{key:?}");
        assert_eq!(state.overlay(), None, "{key:?}");
    }

    // Unknown keys neither commit nor close: the prompt is the only way out.
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
    assert!(update(&mut state, AppEvent::Key(AppKey::Char('z'))).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
    assert!(update(&mut state, AppEvent::Key(AppKey::Escape)).is_empty());
}

#[test]
fn exit_prompt_focus_wraps_in_both_directions_and_enter_commits_it() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());

    // Right and Tab step forward through welcome → quit → stay, wrapping.
    for forward in [AppKey::Right, AppKey::Tab] {
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
        for expected in [ExitChoice::Stay, ExitChoice::Welcome, ExitChoice::Quit] {
            assert!(update(&mut state, AppEvent::Key(forward.clone())).is_empty());
            assert_eq!(state.exit_choice(), expected, "{forward:?}");
            assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
        }
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    }

    // Left steps backward through the same ring.
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
    for expected in [ExitChoice::Welcome, ExitChoice::Stay, ExitChoice::Quit] {
        assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
        assert_eq!(state.exit_choice(), expected);
    }

    // Enter commits whichever button is focused: each of the three in turn.
    for (steps, effects) in [
        (0, vec![Effect::Detach]),
        (1, Vec::new()),
        (2, vec![Effect::LeaveWorkspace]),
    ] {
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
        for _ in 0..steps {
            let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        }
        assert_eq!(update(&mut state, AppEvent::Key(AppKey::Enter)), effects);
        assert_eq!(state.overlay(), None);
    }
}

#[test]
fn exit_choice_order_matches_its_button_indices() {
    for (index, choice) in ExitChoice::ORDER.into_iter().enumerate() {
        assert_eq!(choice.index(), index);
    }
}

#[test]
fn arrow_keys_are_inert_outside_the_quit_confirmation() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let before = state.selected();
    assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Right)).is_empty());
    assert_eq!(state.selected(), before);
    assert_eq!(state.overlay(), None);
}

#[test]
fn overlay_control_chords_follow_the_input_ownership_table() {
    let (workspace, _, _) = ids();
    // `true` means the overlay remains open after the chord. Every overlay
    // owns both keys; only the documented Ctrl-C close contracts dismiss.
    for (overlay, ctrl_c_stays_open, ctrl_q_stays_open) in [
        (Overlay::Overview, true, true),
        (Overlay::Daemon, true, true),
        (Overlay::Closeup, false, true),
        (Overlay::QuitConfirmation, true, true),
        (Overlay::Notes, true, true),
        (Overlay::Environment, true, true),
        (Overlay::CreateSession, true, true),
        (Overlay::Decisions, true, true),
        (Overlay::Prs, true, true),
        (Overlay::Preview, true, true),
        (Overlay::CreateSessionError, false, true),
        (Overlay::TerminalLaunchError, false, true),
        (Overlay::AgentLaunchError, false, true),
    ] {
        for (key, stays_open) in [
            (AppKey::CtrlC, ctrl_c_stays_open),
            (AppKey::CtrlQ, ctrl_q_stays_open),
        ] {
            let mut state = AppState::home(workspace, Vec::new());
            state.overlay = Some(overlay);

            assert!(update(&mut state, AppEvent::Key(key.clone())).is_empty());
            assert_eq!(
                state.overlay(),
                stays_open.then_some(overlay),
                "{overlay:?} {key:?}"
            );
            assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        }
    }
}

#[test]
fn cursor_moves_without_changing_active_target() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
    assert_eq!(state.active(), Some(first));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.active(), Some(second));
}

/// Drive a create to a submitted request and return its pending token.
fn submit_create(state: &mut AppState, name: &[char]) -> PendingToken {
    let _ = update(state, AppEvent::Key(AppKey::CtrlA));
    for character in name {
        let _ = update(state, AppEvent::Key(AppKey::Char(*character)));
    }
    match &update(state, AppEvent::Key(AppKey::Enter))[..] {
        [Effect::CreateSession { token, .. }] => *token,
        other => panic!("expected a single create effect, got {other:?}"),
    }
}

#[test]
fn agent_launch_failure_does_not_replace_an_existing_modal() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);

    let _ = update(
        &mut state,
        AppEvent::AgentLaunchFailed(Notice::new("daemon rejected the Agent")),
    );

    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.agent_launch_error().is_none());
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("daemon rejected the Agent")
    );
}

#[test]
fn fake_backend_records_effects_and_replays_events() {
    let (workspace, first, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let mut backend = FakeControllerBackend::default();
    backend.push_event(BackendEvent::Sessions(vec![first]));
    run_fake_cycle(
        &mut state,
        &mut backend,
        vec![Effect::RefreshSessions { workspace }],
    );
    assert_eq!(backend.effects(), &[Effect::RefreshSessions { workspace }]);
    assert_eq!(state.sessions(), &[first]);
    assert_eq!(
        backend.take_effects(),
        vec![Effect::RefreshSessions { workspace }]
    );
    assert!(backend.effects().is_empty());
}

#[test]
fn future_events_update_only_their_local_state() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(
        &mut state,
        AppEvent::Input(LiveInput::Paste(b"paste".to_vec())),
    );
    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 100,
            height: 40,
        },
    );
    let _ = update(&mut state, AppEvent::Tick);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Notice(Notice::new("connected"))),
    );
    assert_eq!(state.size(), Some((100, 40)));
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("connected")
    );
}

#[test]
fn runtime_stream_converts_to_controller_events() {
    let notice = Notice::new("connected");
    let cases = [
        (
            RuntimeEvent::Input(LiveInput::Paste(b"paste".to_vec())),
            AppEvent::Input(LiveInput::Paste(b"paste".to_vec())),
        ),
        (
            RuntimeEvent::Resize {
                width: 100,
                height: 40,
            },
            AppEvent::Resize {
                width: 100,
                height: 40,
            },
        ),
        (RuntimeEvent::Tick, AppEvent::Tick),
        (
            RuntimeEvent::Backend(BackendEvent::Notice(notice.clone())),
            AppEvent::Backend(BackendEvent::Notice(notice)),
        ),
    ];

    for (runtime, expected) in cases {
        assert_eq!(AppEvent::from(runtime), expected);
    }
}

#[test]
fn phase_projection_isolated_per_runtime_and_uses_the_documented_rank() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let first_a = runtime(workspace, first);
    let first_b = runtime(workspace, first);
    let second_runtime = runtime(workspace, second);

    for (runtime, phase) in [
        (first_a.clone(), AgentPhase::Running),
        (first_b, AgentPhase::Waiting),
        (second_runtime.clone(), AgentPhase::Ready),
    ] {
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase { runtime, phase }),
        );
    }
    assert_eq!(state.runtimes().len(), 3);
    assert_eq!(
        state.phase_for(Target::Session(first)),
        TargetPhase::Waiting
    );
    assert_eq!(state.phase_for(Target::Session(second)), TargetPhase::Ready);

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RuntimePhase {
            runtime: first_a,
            phase: AgentPhase::Ended,
        }),
    );
    assert_eq!(state.runtimes().len(), 3);
    assert_eq!(state.phase_for(Target::Session(first)), TargetPhase::Done);

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RuntimePhase {
            runtime: second_runtime,
            phase: AgentPhase::Exited,
        }),
    );
    assert_eq!(state.phase_for(Target::Session(second)), TargetPhase::Done);
    assert_eq!(
        state.phase_for(Target::Root(workspace)),
        TargetPhase::Absent
    );
}

#[test]
fn feedback_keeps_only_safe_message_and_error_id() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let error = SafeError {
        message: SafeMessage::new("Could not start terminal"),
        error_id: "err-42".to_string(),
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Feedback(Feedback::TerminalError(
            error.clone(),
        ))),
    );
    assert_eq!(state.feedback(), Some(&Feedback::TerminalError(error)));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Feedback(Feedback::Disconnected)),
    );
    assert_eq!(state.feedback(), Some(&Feedback::Disconnected));
}

#[test]
fn navigation_wraps_up_and_ignores_non_command_characters() {
    let (workspace, first, _) = ids();
    let mut state = AppState::home(workspace, vec![first]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.selected(), Selection::NewSession);
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
    assert_eq!(state.selected(), Selection::NewSession);
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlX)).is_empty());
}

#[test]
fn workspace_surfaces_require_reserved_actions_instead_of_plain_letters() {
    let (workspace, session, _) = ids();
    for character in ['p', 'v', 'd'] {
        let mut state = AppState::home(workspace, vec![session]);
        assert!(
            update(&mut state, AppEvent::Key(AppKey::Char(character))).is_empty(),
            "plain {character} must not open a workspace surface"
        );
        assert_eq!(state.overlay(), None);
        assert_eq!(state.pr_overlay(), None);
        assert_eq!(state.preview_overlay(), None);
        assert_eq!(state.decision_overlay(), None);
    }
}

// Removal is never a single unmodified letter. Ctrl-X retries the force
// removal in place; Enter on the failed row still opens the force confirmation.
#[test]
fn failed_delete_ctrl_x_retries_the_force_removal_and_plain_x_is_inert() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Delete),
                failure_summary: Some("safe detail".to_owned()),
            },
        )]))),
    );

    for key in [AppKey::Char('x'), AppKey::Char('X')] {
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
    }
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
    assert_eq!(state.overlay(), None);
    assert_eq!(state.force_remove_confirmation(), None);
    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(session))
    );
}

#[test]
fn selecting_a_delete_failure_confirms_before_forced_removal() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    let failed_delete = SessionLifecycleProjection {
        lifecycle: SessionLifecycle::Failed,
        failure_stage: Some(FailureStage::Delete),
        failure_summary: Some("safe detail".to_owned()),
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            failed_delete,
        )]))),
    );

    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::ForceRemoveConfirmation));
    assert_eq!(state.force_remove_confirmation(), Some((session, true)));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::new())),
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.force_remove_confirmation(), None);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Delete),
                failure_summary: Some("safe detail".to_owned()),
            },
        )]))),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));

    let _ = update(&mut state, AppEvent::Key(AppKey::Right));
    assert_eq!(state.force_remove_confirmation(), Some((session, false)));
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(state.overlay(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Char('y'))),
        vec![Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: true,
            purge_orphan: false,
        }]
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.force_remove_confirmation(), None);
    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(session))
    );
}

#[test]
fn modal_registry_dispatches_once_and_rejects_invalid_or_repeated_requests() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("issue list".to_owned())),
    );
    assert_eq!(
        effects,
        vec![Effect::WorkspaceCommand {
            workspace,
            command: overview::Command::Issue {
                arguments: "list".to_owned(),
            },
        }]
    );
    assert_eq!(state.overlay(), None);
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("issue list".to_owned())),
        )
        .is_empty()
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("unknown".to_owned())),
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("close invalid".to_owned())),
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Closeup));
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("invalid close arguments")
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("terminal open".to_owned())),
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::OpenTerminal { target: Target::Session(actual), arguments, .. }]
            if *actual == session && arguments == "open"
    ));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("terminal open".to_owned())),
        )
        .is_empty()
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitCloseup("terminal new".to_owned())),
    );
    assert_eq!(
        effects,
        vec![Effect::OpenExternalTerminal {
            target: Target::Session(session),
        }]
    );
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(state.overlay(), None);
    assert!(!state.closeup_action_forced);
}

#[test]
fn fake_port_keeps_note_and_environment_edits_on_safe_failures() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let mut backend = FakeControllerBackend::default();

    let effects = update(&mut state, AppEvent::Key(AppKey::OpenNotes));
    assert_eq!(effects, vec![Effect::LoadNotes { target }]);
    backend.push_event(BackendEvent::NotesLoaded {
        target,
        scratchpad: Scratchpad {
            note: Some("before".to_owned()),
            todos: vec![usagi_core::domain::note::SessionTodo::new("test it")],
            decisions: Vec::new(),
        },
    });
    run_fake_cycle(&mut state, &mut backend, effects);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SelectNoteSection(NoteSection::Todos)),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleTodo(0)));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SetNoteDraft("document it".to_owned())),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::CommitNoteDraft));
    let queued_saves = update(&mut state, AppEvent::Key(AppKey::SaveNotes));
    assert!(
        matches!(&queued_saves[..], [Effect::SaveNotes { target: saved_target, scratchpad }] if *saved_target == target && scratchpad.todos.len() == 2 && scratchpad.todos[0].done)
    );
    backend.push_event(BackendEvent::NotesError {
        target,
        error: SafeError {
            message: SafeMessage::new("Could not save notes"),
            error_id: "safe-note-1".to_owned(),
        },
    });
    run_fake_cycle(&mut state, &mut backend, queued_saves);
    let note = state.note_editor().unwrap();
    assert_eq!(note.scratchpad().todos[1].text, "document it");
    assert_eq!(
        note.error().unwrap().message.as_str(),
        "Could not save notes"
    );

    let effects = update(&mut state, AppEvent::Key(AppKey::OpenEnvironment));
    assert_eq!(
        effects,
        vec![Effect::LoadEnvironment {
            scope: EnvScope::Workspace
        }]
    );
    backend.push_event(BackendEvent::EnvironmentLoaded {
        scope: EnvScope::Workspace,
        entries: vec![EnvironmentEntry {
            name: "MODE".to_owned(),
            value: "dev".to_owned(),
        }],
        inherited: Vec::new(),
    });
    run_fake_cycle(&mut state, &mut backend, effects);
    let editor = state.environment_editor.as_mut().unwrap();
    editor.source.replace("MODE=test");
    let saves = update(&mut state, AppEvent::Key(AppKey::SaveEnvironment));
    assert_eq!(
        saves,
        vec![Effect::SaveEnvironment {
            scope: EnvScope::Workspace,
            entries: vec![EnvironmentEntry {
                name: "MODE".to_owned(),
                value: "test".to_owned()
            }],
        }]
    );
    backend.push_event(BackendEvent::EnvironmentError {
        scope: EnvScope::Workspace,
        error: SafeError {
            message: SafeMessage::new("Could not save environment"),
            error_id: "safe-env-1".to_owned(),
        },
    });
    run_fake_cycle(&mut state, &mut backend, saves);
    let environment = state.environment_editor().unwrap();
    assert_eq!(environment.entries()[0].value, "test");
    assert_eq!(
        environment.error().unwrap().message.as_str(),
        "Could not save environment"
    );
}

fn entry(name: &str, value: &str) -> EnvironmentEntry {
    EnvironmentEntry {
        name: name.to_owned(),
        value: value.to_owned(),
    }
}

#[test]
fn daemon_modal_runs_one_non_force_action_and_fences_its_completion() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("daemon".into())),
    );

    assert_eq!(state.daemon_control().selected(), DaemonAction::Restart);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.daemon_control().selected(), DaemonAction::Stop);
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(
        effects,
        vec![Effect::DaemonControl {
            workspace,
            action: DaemonAction::Stop,
            token: PendingToken(1),
        }]
    );
    assert_eq!(state.daemon_control().pending(), Some(DaemonAction::Stop));

    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Up)).is_empty());
    assert_eq!(state.daemon_control().selected(), DaemonAction::Stop);

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DaemonControlFinished {
            workspace,
            action: DaemonAction::Start,
            token: PendingToken(99),
            result: Ok(Notice::new("stale")),
        }),
    );
    assert_eq!(state.daemon_control().pending(), Some(DaemonAction::Stop));

    let refused = SafeError {
        message: SafeMessage::new("Stop failed: live runtimes are active"),
        error_id: "daemon-stop-failed".to_owned(),
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DaemonControlFinished {
            workspace,
            action: DaemonAction::Stop,
            token: PendingToken(1),
            result: Err(refused.clone()),
        }),
    );
    assert_eq!(state.daemon_control().pending(), None);
    assert_eq!(state.daemon_control().result(), Some(&Err(refused)));
    assert_eq!(state.overlay(), Some(Overlay::Daemon));

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("daemon".into())),
    );
    assert_eq!(state.daemon_control(), &DaemonControlState::default());
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Char('s'))),
        vec![Effect::DaemonControl {
            workspace,
            action: DaemonAction::Start,
            token: PendingToken(2),
        }]
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("daemon".into())),
    );
    assert!(matches!(
        update(&mut state, AppEvent::Key(AppKey::Char('s'))).as_slice(),
        [Effect::DaemonControl {
            token: PendingToken(3),
            ..
        }]
    ));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DaemonControlFinished {
            workspace,
            action: DaemonAction::Start,
            token: PendingToken(2),
            result: Ok(Notice::new("old completion")),
        }),
    );
    assert_eq!(
        state.daemon_control.pending,
        Some((DaemonAction::Start, PendingToken(3)))
    );
}

#[test]
fn daemon_modal_navigation_wraps_and_stop_shortcut_is_direct() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("daemon".into())),
    );

    for (key, expected) in [
        (AppKey::Up, DaemonAction::Start),
        (AppKey::Up, DaemonAction::Stop),
        (AppKey::Down, DaemonAction::Start),
        (AppKey::Left, DaemonAction::Stop),
    ] {
        let _ = update(&mut state, AppEvent::Key(key));
        assert_eq!(state.daemon_control().selected(), expected);
    }
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Char('x'))),
        vec![Effect::DaemonControl {
            workspace,
            action: DaemonAction::Stop,
            token: PendingToken(1),
        }]
    );
}

/// A Failed checkout is not usable, so the garden's visit refuses to attach
/// it exactly as the sidebar's activation does. The garden adds no target
/// semantics of its own.
#[test]
fn visiting_a_failed_usagi_selects_it_without_attaching_it() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            session,
            lifecycle(SessionLifecycle::Failed),
        )]))),
    );
    state.overlay = Some(Overlay::Garden);

    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Visit {
                workspace,
                session,
                agent: None,
            })
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
}

#[test]
fn role_editor_reducer_keeps_invalid_source_and_switches_scopes() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);
    let effects = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("roles workspace".into())),
    );
    assert_eq!(
        effects,
        vec![Effect::LoadRoles {
            scope: RoleEditorScope::Workspace
        }]
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RolesLoaded {
            scope: RoleEditorScope::Workspace,
            source: "version = 1\n# preserved".into(),
        }),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
    let saves = update(&mut state, AppEvent::Key(AppKey::SaveRoles));
    assert!(
        matches!(saves.as_slice(), [Effect::SaveRoles { scope: RoleEditorScope::Workspace, source }]
            if source.ends_with('x'))
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RolesError {
            scope: RoleEditorScope::Workspace,
            error: SafeError {
                message: SafeMessage::new("invalid role catalog"),
                error_id: "roles-invalid".into(),
            },
        }),
    );
    assert!(state.role_editor().unwrap().source().ends_with('x'));
    assert_eq!(
        state
            .role_editor()
            .unwrap()
            .error()
            .unwrap()
            .message
            .as_str(),
        "invalid role catalog"
    );
    let switched = update(&mut state, AppEvent::Key(AppKey::Tab));
    assert_eq!(
        switched,
        vec![Effect::LoadRoles {
            scope: RoleEditorScope::Global
        }]
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RolesLoaded {
            scope: RoleEditorScope::Global,
            source: "version = 1\n".into(),
        }),
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::ToggleRoleScope)),
        vec![Effect::LoadRoles {
            scope: RoleEditorScope::Workspace
        }]
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), None);

    state.overlay = Some(Overlay::Roles);
    state.role_editor = None;
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(state.overlay(), None);

    state.overlay = Some(Overlay::Overview);
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("roles invalid".into()))
        )
        .is_empty()
    );
    assert!(
        state
            .notice()
            .unwrap()
            .message
            .as_str()
            .contains("roles takes")
    );
}

#[test]
fn role_editor_scrolls_long_sources_and_follows_tail_edits() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("roles workspace".into())),
    );
    let source = (0..30)
        .map(|line| format!("line-{line:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RolesLoaded {
            scope: RoleEditorScope::Workspace,
            source,
        }),
    );

    assert_eq!(state.role_editor().unwrap().scroll_top(), 16);
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.role_editor().unwrap().scroll_top(), 15);
    let _ = update(&mut state, AppEvent::Key(AppKey::PageUp));
    assert_eq!(state.role_editor().unwrap().scroll_top(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::PageUp));
    assert_eq!(state.role_editor().unwrap().scroll_top(), 0);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::PageDown));
    let _ = update(&mut state, AppEvent::Key(AppKey::PageDown));
    assert_eq!(state.role_editor().unwrap().scroll_top(), 16);

    let _ = update(&mut state, AppEvent::Key(AppKey::PageUp));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
    assert_eq!(state.role_editor().unwrap().scroll_top(), 17);
    assert!(state.role_editor().unwrap().source().ends_with("\nx"));
}

#[test]
fn overview_global_env_uses_ctrl_s_source_save_and_ignores_tab() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("env global".to_owned())),
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::EnvironmentLoaded {
            scope: EnvScope::Global,
            entries: vec![entry("TOKEN", "secret")],
            inherited: Vec::new(),
        }),
    );

    assert_eq!(state.environment_editor().unwrap().draft(), "TOKEN=secret");
    assert!(update(&mut state, AppEvent::Key(AppKey::Tab)).is_empty());
    assert!(!state.environment_editor().unwrap().is_save_focused());
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::SaveRoles)),
        vec![Effect::SaveEnvironment {
            scope: EnvScope::Global,
            entries: vec![entry("TOKEN", "secret")],
        }]
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::EnvironmentSaved {
            scope: EnvScope::Global,
            entries: vec![entry("TOKEN", "secret")],
            inherited: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.environment_editor().is_none());
}

#[test]
fn overview_config_targets_the_workspace_and_rejects_extra_arguments() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));

    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("config".to_owned())),
        ),
        vec![Effect::WorkspaceCommand {
            workspace,
            command: overview::Command::Config {
                arguments: String::new(),
            },
        }]
    );
    assert_eq!(state.overlay(), None);

    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("config extra".to_owned())),
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("config takes no arguments (usage: config)")
    );
}

#[test]
fn overview_clean_requires_explicit_apply_before_force() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert_eq!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("clean --apply".to_owned())),
        ),
        vec![Effect::WorkspaceCommand {
            workspace,
            command: overview::Command::Clean {
                arguments: "--apply".to_owned(),
            },
        }]
    );
    assert_eq!(state.overlay(), None);

    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("clean --force".to_owned())),
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert_eq!(
        state.notice().map(|notice| notice.message.as_str()),
        Some("invalid clean arguments (usage: clean [--apply [--force]])")
    );
}

fn pending_decision(workspace: WorkspaceId) -> UserDecision {
    UserDecision {
        decision_id: UserDecisionId::new(),
        owner: usagi_core::domain::user_decision::UserDecisionOwner {
            workspace_id: workspace,
            session_id: Some(SessionId::new()),
            caller: usagi_core::domain::agent::CallerRef {
                session_id: Some(SessionId::new()),
                agent_id: usagi_core::domain::id::AgentId::new(),
            },
            run_id: OperationId::new(),
        },
        title: "Choose a path".into(),
        prompt: "Which path?".into(),
        options: vec![usagi_core::domain::user_decision::UserDecisionOption {
            id: "safe".into(),
            label: "Safe".into(),
            description: Some("Keeps current state".into()),
        }],
        allow_freeform: false,
        expires_at: None,
        idempotency_key: None,
        status: UserDecisionStatus::Pending,
        answer: None,
        created_at: chrono::Utc::now(),
        resolved_at: None,
    }
}

#[test]
fn paste_is_inserted_into_every_reducer_owned_home_input() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();

    let mut notes = AppState::home(workspace, vec![session]);
    let _ = update(&mut notes, AppEvent::Key(AppKey::OpenNotes));
    let _ = update(
        &mut notes,
        AppEvent::Key(AppKey::SetNoteDraft("before ".to_owned())),
    );
    let _ = update(
        &mut notes,
        AppEvent::Key(AppKey::Paste("貼\n付".to_owned())),
    );
    assert_eq!(notes.note_editor().unwrap().draft(), "before 貼\n付");

    let mut environment = AppState::home(workspace, vec![session]);
    let _ = update(&mut environment, AppEvent::Key(AppKey::OpenEnvironment));
    environment.environment_editor.as_mut().unwrap().loading = false;
    let editor = environment.environment_editor.as_mut().unwrap();
    editor.source.replace("TOKEN=");
    let _ = update(
        &mut environment,
        AppEvent::Key(AppKey::Paste("long-value".to_owned())),
    );
    assert_eq!(
        environment.environment_editor().unwrap().draft(),
        "TOKEN=long-value"
    );

    let mut roles = AppState::home(workspace, Vec::new());
    roles.overlay = Some(Overlay::Roles);
    roles.role_editor = Some(RoleEditor {
        scope: RoleEditorScope::Workspace,
        source: "version = 1\n".to_owned(),
        error: None,
        loading: false,
        saving: false,
        scroll_top: 0,
    });
    let _ = update(
        &mut roles,
        AppEvent::Key(AppKey::Paste("[roles.dev]\nmodel = \"codex\"\n".to_owned())),
    );
    assert_eq!(
        roles.role_editor().unwrap().source(),
        "version = 1\n[roles.dev]\nmodel = \"codex\"\n"
    );

    let mut create = AppState::home(workspace, vec![session]);
    let _ = update(&mut create, AppEvent::Key(AppKey::CtrlA));
    let _ = update(&mut create, AppEvent::Key(AppKey::Char('a')));
    let _ = update(
        &mut create,
        AppEvent::Key(AppKey::Paste("-pasted".to_owned())),
    );
    assert_eq!(create.create_session_form().unwrap().name(), "a-pasted");

    let mut decision = pending_decision(workspace);
    decision.allow_freeform = true;
    let mut decisions = AppState::home(workspace, Vec::new());
    let _ = update(&mut decisions, AppEvent::Key(AppKey::OpenDecisions));
    let _ = update(
        &mut decisions,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions: vec![decision],
        }),
    );
    let _ = update(&mut decisions, AppEvent::Key(AppKey::Enter));
    let _ = update(
        &mut decisions,
        AppEvent::Key(AppKey::Paste("free form".to_owned())),
    );
    assert_eq!(
        decisions
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .freeform(),
        "free form"
    );
}

fn pr_link(number: u32) -> PrEntry {
    let identity = usagi_core::domain::pr_inventory::canonicalize(&format!(
        "https://github.com/o/r/pull/{number}"
    ))
    .unwrap();
    let mut pr = PrEntry::new(identity);
    pr.auto_open = true;
    pr
}

fn merged_pr_link(number: u32) -> PrEntry {
    let mut pr = pr_link(number);
    pr.state = PrState::Merged;
    pr
}

fn observe_prs(state: &mut AppState, session: SessionId, revision: u64, prs: Vec<PrEntry>) {
    let _ = update(
        state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target: Target::Session(session),
            revision,
            prs,
        }),
    );
}

#[test]
fn cleanup_queue_serializes_selected_removals_by_stable_identity() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    observe_prs(&mut state, first, 1, vec![merged_pr_link(1)]);
    observe_prs(&mut state, second, 1, vec![merged_pr_link(2)]);
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("session cleanup".to_owned())),
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Char('a')));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::RemoveSession {
            workspace,
            session: first,
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        }]
    );
    assert_eq!(state.cleanup_queue().unwrap().in_flight(), Some(first));

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
                force: false,
                force_delete_branch: false,
                purge_orphan: false,
            },
        ]
    );
    assert_eq!(state.cleanup_queue().unwrap().in_flight(), Some(second));

    assert_eq!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(Vec::new()))
        ),
        vec![Effect::SyncPullRequestTargets {
            sessions: Vec::new()
        }]
    );
    let queue = state.cleanup_queue().unwrap();
    assert!(queue.candidates().is_empty());
    assert_eq!(queue.in_flight(), None);
    assert_eq!(queue.feedback().unwrap().message, "cleanup complete");
}

#[test]
fn cleanup_queue_navigation_waiting_and_failed_delete_paths_are_explicit() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    observe_prs(&mut state, first, 1, vec![merged_pr_link(1)]);
    observe_prs(&mut state, second, 1, vec![merged_pr_link(2)]);
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SubmitOverview("session cleanup".to_owned())),
    );

    assert_eq!(state.cleanup_queue().unwrap().cursor(), 0);
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(
        state.cleanup_queue().unwrap().feedback().unwrap().message,
        "select sessions with Space"
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.cleanup_queue().unwrap().cursor(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.cleanup_queue().unwrap().cursor(), 0);

    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    assert_eq!(
        state.cleanup_queue().unwrap().selected(),
        &BTreeSet::from([first])
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Char(' ')));
    assert!(state.cleanup_queue().unwrap().selected().is_empty());

    let _ = update(&mut state, AppEvent::Key(AppKey::Char('a')));
    assert_eq!(state.cleanup_queue().unwrap().selected().len(), 2);
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('A')));
    assert!(state.cleanup_queue().unwrap().selected().is_empty());
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('a')));
    assert!(matches!(
        update(&mut state, AppEvent::Key(AppKey::Enter)).as_slice(),
        [Effect::RemoveSession { session, .. }] if *session == first
    ));
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    assert_eq!(
        state.cleanup_queue().unwrap().feedback().unwrap().message,
        "waiting for the current removal"
    );

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::SessionLifecycles(BTreeMap::from([(
            first,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(FailureStage::Delete),
                failure_summary: Some("safe detail".to_owned()),
            },
        )]))),
    );
    let queue = state.cleanup_queue().unwrap();
    assert_eq!(queue.in_flight(), None);
    assert!(!queue.selected().contains(&first));
    assert_eq!(
        queue.feedback().unwrap().message,
        "cleanup paused after removal failed"
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), None);
    assert_eq!(state.cleanup_queue(), None);

    state.overlay = Some(Overlay::CleanupQueue);
    assert!(update_cleanup_queue(&mut state, &AppKey::Down).is_empty());
    assert_eq!(state.overlay(), None);
    assert!(begin_next_cleanup(&mut state, false).is_empty());

    state.overlay = Some(Overlay::CleanupQueue);
    state.cleanup_queue = Some(CleanupQueueState::new(Vec::new()));
    assert!(update_cleanup_queue(&mut state, &AppKey::Char(' ')).is_empty());
    assert!(state.cleanup_queue().unwrap().selected().is_empty());
}

fn safe_error(message: &str) -> SafeError {
    SafeError {
        message: SafeMessage::new(message),
        error_id: "overlay".into(),
    }
}

fn preview_loaded(
    state: &AppState,
    target: Target,
    path: Option<&str>,
    files: &[&str],
    lines: &[&str],
) -> AppEvent {
    AppEvent::Backend(BackendEvent::PreviewLoaded {
        target,
        request_id: state.preview_overlay().unwrap().request_id(),
        path: path.map(str::to_owned),
        filter: state.preview_overlay().unwrap().file_filter(),
        files: files.iter().map(ToString::to_string).collect(),
        lines: lines.iter().map(ToString::to_string).collect(),
    })
}

fn escape_preview(state: &mut AppState) {
    assert_eq!(
        update(state, AppEvent::Key(AppKey::Escape)),
        vec![Effect::CancelPreview]
    );
}

fn assert_preview_load(
    state: &AppState,
    effects: &[Effect],
    target: Target,
    path: Option<&str>,
    filter: PreviewFileFilter,
) {
    assert_eq!(
        effects,
        [Effect::LoadPreview {
            target,
            request_id: state.preview_overlay().unwrap().request_id(),
            path: path.map(str::to_owned),
            filter,
        }]
    );
}

#[test]
fn preview_overlay_resolves_its_target_from_the_home_mode() {
    let (workspace, active, selected) = ids();
    let missing = SessionId::new();
    let mut state = AppState::home(workspace, vec![active, selected]);
    let _ = update(&mut state, AppEvent::Key(AppKey::NextSession));
    assert_eq!(state.active(), Some(active));
    assert_eq!(
        state.selected(),
        Selection::Target(Target::Session(selected))
    );

    let effects = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
    let request_id = state.preview_overlay().unwrap().request_id();
    assert_eq!(
        effects,
        vec![Effect::LoadPreview {
            target: Target::Session(selected),
            request_id,
            path: None,
            filter: PreviewFileFilter::All,
        }]
    );
    assert_eq!(
        state.preview_overlay().map(PreviewOverlay::target),
        Some(Target::Session(selected))
    );

    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    state.route = Route::Home(HomeMode::Closeup);
    let effects = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
    let request_id = state.preview_overlay().unwrap().request_id();
    assert_eq!(
        effects,
        vec![Effect::LoadPreview {
            target: Target::Session(active),
            request_id,
            path: None,
            filter: PreviewFileFilter::All,
        }]
    );
    assert_eq!(
        state.preview_overlay().map(PreviewOverlay::target),
        Some(Target::Session(active))
    );

    for selection in [
        Selection::NewSession,
        Selection::Idle,
        Selection::Target(Target::Root(workspace)),
        Selection::Target(Target::Session(missing)),
    ] {
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        state.route = Route::Home(HomeMode::Switch);
        state.selected = selection;
        assert!(update(&mut state, AppEvent::Key(AppKey::OpenPreview)).is_empty());
        assert_eq!(state.overlay(), None);
    }

    state.route = Route::Home(HomeMode::Closeup);
    state.active = Some(missing);
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenPreview)).is_empty());
    assert_eq!(state.overlay(), None);
    state.active = None;
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenPreview)).is_empty());
    assert_eq!(state.overlay(), None);
}

#[test]
fn decision_editor_covers_freeform_navigation_and_invalid_answers() {
    let workspace = WorkspaceId::new();
    let mut decision = pending_decision(workspace);
    decision.options.clear();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDecisions));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions: vec![decision.clone()],
        }),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert!(update(&mut state, AppEvent::Key(AppKey::SubmitDecision)).is_empty());
    assert_eq!(
        state
            .decision_overlay()
            .unwrap()
            .editor()
            .unwrap()
            .error()
            .unwrap()
            .error_id,
        "decision-invalid-answer"
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));

    decision.allow_freeform = true;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions: vec![decision.clone()],
        }),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
    let _ = update(&mut state, AppEvent::Key(AppKey::Backspace));
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::SetDecisionFreeform(" answer ".to_owned())),
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::ResolveDecision {
            workspace,
            decision_id: decision.decision_id,
            answer: UserDecisionAnswer::Freeform {
                text: "answer".to_owned(),
            },
        }]
    );
}

#[test]
#[allow(clippy::too_many_lines)] // The reducer matrix shares one state and preserves event order.
fn coverage_contract_exercises_reducer_noop_error_and_reconcile_paths() {
    let (workspace, session, _) = ids();
    assert_eq!(TargetPhase::Absent.rank(), 0);
    assert_eq!(
        TargetPhase::from_agent_phase(AgentPhase::Absent),
        TargetPhase::Absent
    );
    assert_eq!(
        NewValidationError::DirectoryInvalid.message(),
        "directory name must be a safe name without path separators"
    );

    let mut entry = EntryState::new(
        vec![EntryWorkspace::new(workspace, "repo")],
        vec![workspace],
    );
    assert_eq!(
        update_entry(&mut entry, EntryEvent::OpenRecent(workspace)).len(),
        1
    );
    assert!(update_entry(&mut entry, EntryEvent::OpenRecent(workspace)).is_empty());

    let mut new = NewState::new(NewMode::Existing, existing_form());
    assert_eq!(update_new(&mut new, NewEvent::Submit).len(), 1);
    assert!(update_new(&mut new, NewEvent::Submit).is_empty());
    assert!(
        new.request(NewRequest::Existing {
            path: PathBuf::from("/work/existing"),
            name: "existing".to_owned(),
        })
        .is_empty()
    );

    let mut state = AppState::home(workspace, vec![session]);
    assert!(!update_editor_backend(
        &mut state,
        &BackendEvent::Decisions {
            workspace,
            decisions: Vec::new(),
        },
    ));
    assert!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::DecisionResolved {
                workspace: WorkspaceId::new(),
                decision_id: UserDecisionId::new(),
            })
        )
        .is_empty()
    );
    for event in [
        BackendEvent::NotesLoaded {
            target: Target::Session(session),
            scratchpad: Scratchpad::default(),
        },
        BackendEvent::NotesError {
            target: Target::Session(session),
            error: safe_error("notes"),
        },
        BackendEvent::EnvironmentLoaded {
            scope: EnvScope::Workspace,
            entries: Vec::new(),
            inherited: Vec::new(),
        },
        BackendEvent::EnvironmentError {
            scope: EnvScope::Workspace,
            error: safe_error("env"),
        },
        BackendEvent::PullRequestsLoaded {
            target: Target::Session(session),
            revision: 1,
            prs: Vec::new(),
        },
        BackendEvent::PullRequestsError {
            target: Target::Session(session),
            error: safe_error("prs"),
        },
        BackendEvent::PreviewLoaded {
            target: Target::Session(session),
            request_id: RequestId::new(),
            path: None,
            filter: PreviewFileFilter::All,
            files: Vec::new(),
            lines: Vec::new(),
        },
        BackendEvent::PreviewError {
            target: Target::Session(session),
            request_id: RequestId::new(),
            path: None,
            filter: PreviewFileFilter::All,
            error: safe_error("preview"),
        },
    ] {
        assert!(update(&mut state, AppEvent::Backend(event)).is_empty());
    }

    state.ctrl_c_grace = true;
    state.route = Route::Home(HomeMode::Closeup);
    assert!(update_key(&mut state, AppKey::CtrlC).is_empty());
    state.ctrl_c_grace = false;
    assert_eq!(update_key(&mut state, AppKey::CtrlC), vec![Effect::Detach]);

    state.overlay = Some(Overlay::QuitConfirmation);
    let _ = update_overlay(&mut state, Overlay::QuitConfirmation, AppKey::Home);
    let _ = update_overlay(&mut state, Overlay::QuitConfirmation, AppKey::Tab);
    state.overlay = Some(Overlay::CreateSessionError);
    let _ = update_overlay(&mut state, Overlay::CreateSessionError, AppKey::Home);

    state.overlay = Some(Overlay::Prs);
    state.pr_overlay = None;
    assert!(pull_requests::update_key(&mut state, &AppKey::Enter).is_empty());
    state.overlay = Some(Overlay::Preview);
    state.preview_overlay = None;
    assert_eq!(
        preview::update_preview_overlay(&mut state, &AppKey::Enter),
        vec![Effect::CancelPreview]
    );

    assert!(commit_note_draft(&mut state).is_empty());
    state.overlay = Some(Overlay::Notes);
    state.note_editor = Some(NoteEditor::loading(Target::Root(workspace)));
    for section in [
        NoteSection::Note,
        NoteSection::Todos,
        NoteSection::Decisions,
    ] {
        let editor = state.note_editor.as_mut().unwrap();
        editor.section = section;
        editor.draft.clear();
        assert!(commit_note_draft(&mut state).is_empty());
    }

    let decision = pending_decision(workspace);
    state.decisions = vec![decision.clone()];
    state.decision_overlay = Some(DecisionOverlayState {
        selected: 0,
        editor: Some(DecisionEditor::new(decision)),
    });
    state.decisions.clear();
    reconcile_decision_overlay(&mut state);
    assert!(state.decision_overlay.as_ref().unwrap().editor.is_none());

    state.overlay = Some(Overlay::CreateSession);
    state.create_session = None;
    assert!(update_create_session_form(&mut state, &AppKey::Enter).is_empty());
    state.create_session = Some(CreateSessionForm::new(Vec::new()));
    assert!(update_create_session_form(&mut state, &AppKey::Backspace).is_empty());

    state.overlay = Some(Overlay::Notes);
    state.note_editor = Some(NoteEditor::loading(Target::Root(workspace)));
    let _ = update_overlay(&mut state, Overlay::Notes, AppKey::Escape);
    state.overlay = Some(Overlay::Environment);
    state.environment_editor = Some(EnvironmentEditor::loading(EnvScope::Workspace));
    let _ = update_overlay(&mut state, Overlay::Environment, AppKey::Escape);

    let mut decision = pending_decision(workspace);
    decision.allow_freeform = true;
    state.decisions = vec![decision];
    state.overlay = Some(Overlay::Decisions);
    state.decision_overlay = Some(DecisionOverlayState {
        selected: 0,
        editor: None,
    });
    for key in [
        AppKey::Up,
        AppKey::Down,
        AppKey::Home,
        AppKey::Enter,
        AppKey::Up,
        AppKey::Down,
        AppKey::SetDecisionFreeform("answer".into()),
        AppKey::Backspace,
        AppKey::Char('x'),
    ] {
        let _ = update_decisions_overlay(&mut state, key);
    }
    assert!(matches!(
        update_decisions_overlay(&mut state, AppKey::SubmitDecision).as_slice(),
        [Effect::ResolveDecision { .. }]
    ));
    let _ = update_decisions_overlay(&mut state, AppKey::Escape);
    let _ = update_decisions_overlay(&mut state, AppKey::Escape);

    for command in [
        "env extra",
        "session list",
        "session overview",
        "session remove -s",
        "session remove named",
        "issue",
        "unknown",
    ] {
        state.overlay = Some(Overlay::Overview);
        let _ = submit_overview(&mut state, command);
    }
    for command in [
        "terminal bad",
        "agent too many args",
        "close bad",
        "diff",
        "unknown",
    ] {
        state.overlay = Some(Overlay::Closeup);
        let _ = submit_closeup(&mut state, command);
    }

    state.overlay = Some(Overlay::Prs);
    state.pr_overlay = Some(PrOverlay::showing(
        Target::Session(session),
        Vec::new(),
        None,
    ));
    for key in [AppKey::Up, AppKey::Down, AppKey::Home] {
        let _ = pull_requests::update_key(&mut state, &key);
    }

    state.decision_overlay = None;
    assert!(update_decisions_overlay(&mut state, AppKey::Escape).is_empty());
    let mut decision = pending_decision(workspace);
    decision.expires_at = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
    state.decisions = vec![decision.clone()];
    state.decision_overlay = Some(DecisionOverlayState {
        selected: 0,
        editor: Some(DecisionEditor::new(decision)),
    });
    let _ = update_decisions_overlay(&mut state, AppKey::Enter);
    let _ = update_decisions_overlay(&mut state, AppKey::Home);
    state.decision_overlay.as_mut().unwrap().editor = None;
    let _ = update_decisions_overlay(&mut state, AppKey::Escape);

    let first = pending_decision(workspace);
    let second = pending_decision(workspace);
    state.decisions = vec![first.clone(), second.clone()];
    state.decision_overlay = Some(DecisionOverlayState {
        selected: 1,
        editor: Some(DecisionEditor::new(second.clone())),
    });
    reconcile_decision_overlay(&mut state);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DecisionResolved {
            workspace,
            decision_id: first.decision_id,
        }),
    );
    assert_eq!(
        state
            .decision_overlay
            .as_ref()
            .and_then(|overlay| overlay.editor.as_ref())
            .map(|editor| editor.decision.decision_id),
        Some(second.decision_id)
    );

    state.overlay = Some(Overlay::Notes);
    state.note_editor = Some(NoteEditor::loading(Target::Root(workspace)));
    assert!(update_editor_key(&mut state, &AppKey::Home).is_none());
    for (section, draft) in [
        (NoteSection::Note, "note"),
        (NoteSection::Todos, "todo"),
        (NoteSection::Decisions, "decision"),
    ] {
        let editor = state.note_editor.as_mut().unwrap();
        editor.section = section;
        editor.draft = draft.into();
        let _ = commit_note_draft(&mut state);
    }
    state.preview_overlay = None;
    let _ = preview::update_preview_overlay(&mut state, &AppKey::Home);
    state.preview_overlay = Some(PreviewOverlay::loading(Target::Root(workspace)));
    let _ = preview::update_preview_overlay(&mut state, &AppKey::Home);

    state.overlay = Some(Overlay::Environment);
    state.environment_editor = Some(EnvironmentEditor {
        scope: EnvScope::Workspace,
        entries: Vec::new(),
        source: EnvironmentSourceEditor::default(),
        loading: false,
        saving: false,
        error: None,
    });
    state.overlay = Some(Overlay::Overview);
    let _ = submit_overview(&mut state, "session create created");
    state.overlay = Some(Overlay::Overview);
    let _ = submit_overview(&mut state, "session create");
    state.overlay = Some(Overlay::Closeup);
    let _ = submit_closeup(&mut state, "diff status");
    state.active = Some(session);
    state.overlay = Some(Overlay::Closeup);
    let _ = submit_closeup(&mut state, "close invalid");
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::NotesLoaded {
            target: Target::Root(WorkspaceId::new()),
            scratchpad: Scratchpad::default(),
        }),
    );
}

#[test]
fn coverage_contract_executes_guarded_reducer_success_paths() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    state.select_row(Selection::Target(Target::Session(session)));

    let choice = EntryWorkspace::new(workspace, "demo");
    let mut entry = EntryState::new(vec![choice.clone()], vec![workspace]);
    let _ = update_entry(&mut entry, EntryEvent::ShowOpen);
    let _ = update_entry(&mut entry, EntryEvent::OpenSingle(workspace));
    let mut entry = EntryState::new(vec![choice], vec![workspace]);
    let _ = update_entry(&mut entry, EntryEvent::OpenRecent(workspace));
    let mut entry = EntryState::new(Vec::new(), Vec::new());
    let _ = update_entry(&mut entry, EntryEvent::ShowOpen);
    let _ = update_entry(&mut entry, EntryEvent::Back);

    let mut new = NewState::new(NewMode::Existing, existing_form());
    let effects = update_new(&mut new, NewEvent::Submit);
    let token = match effects.as_slice() {
        [Effect::RegisterWorkspace { token, .. }] => *token,
        other => panic!("unexpected new effect: {other:?}"),
    };
    let _ = update_new(
        &mut new,
        NewEvent::Result {
            token,
            result: Err(Notice::new("failed")),
        },
    );
    let _ = update_new(&mut new, NewEvent::Retry);

    let decision = pending_decision(workspace);
    state.decisions = vec![decision.clone()];
    state.decision_overlay = Some(DecisionOverlayState {
        selected: 0,
        editor: Some(DecisionEditor::new(decision.clone())),
    });
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::DecisionError {
            workspace,
            decision_id: decision.decision_id,
            error: safe_error("retry"),
        }),
    );
    let _ = update_decisions_overlay(&mut state, AppKey::SetDecisionFreeform("answer".to_owned()));
    state.decision_overlay.as_mut().unwrap().editor = None;
    let _ = update_decisions_overlay(&mut state, AppKey::Enter);

    state.route = Route::Home(HomeMode::Closeup);
    state.overlay = Some(Overlay::Closeup);
    state.closeup_action_forced = false;
    state.has_live_pane = false;
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    assert!(!update_management_key(&mut state, AppKey::CtrlN).is_empty());
    assert!(!update_management_key(&mut state, AppKey::CtrlP).is_empty());

    state.route = Route::Home(HomeMode::Switch);
    state.overlay = None;
    state.selected = Selection::Target(Target::Session(session));
    let _ = update_management_key(&mut state, AppKey::Char('x'));

    state.overlay = Some(Overlay::Notes);
    state.note_editor = Some(NoteEditor::loading(Target::Session(session)));
    state.note_editor.as_mut().unwrap().scratchpad.todos =
        vec![usagi_core::domain::note::SessionTodo::new("covered")];
    for key in [
        AppKey::SelectNoteSection(NoteSection::Todos),
        AppKey::SetNoteDraft("draft".to_owned()),
        AppKey::ToggleTodo(0),
    ] {
        let _ = update_editor_key(&mut state, &key);
    }
}

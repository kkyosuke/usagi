#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
use super::*;
use crate::usecase::application::environment_source::parse_environment_source;
use std::collections::VecDeque;

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
fn workflow_recovers_pending_start_and_accepts_instruction_completion() {
    use crate::usecase::application::workflow::{WorkflowJob, fixture_run};
    use usagi_core::domain::workflow::{
        Recipient, WorkflowCommand, WorkflowPendingStart, WorkflowSnapshot,
    };
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let operation = OperationId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let _ = submit_closeup_workflow(&mut state, session, "");
    let job = WorkflowJob {
        workspace,
        session,
        control: None,
    };
    let snapshot = WorkflowSnapshot {
        agents: usagi_core::domain::workflow::WorkflowAgents::default(),
        session,
        run: None,
        pending_start: Some(WorkflowPendingStart {
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            operation_id: operation,
            goal: "Build login".into(),
            error: Some("Sign in to retry".into()),
        }),
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(snapshot.clone())),
        }),
    );
    let panel = state.workflow_panel(session).unwrap();
    assert_eq!(panel.draft.value(), "Build login");
    assert_eq!(panel.error.as_deref(), Some("Sign in to retry"));
    assert_eq!(
        panel.pending,
        Some((
            operation,
            WorkflowCommand::Start {
                goal: "Build login".into(),
                agents: usagi_core::domain::workflow::WorkflowAgents::default()
            }
        ))
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(snapshot)),
        }),
    );
    let effects = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    assert!(
        matches!(&effects[0], Effect::Workflow(job) if job.control.as_ref().unwrap().0 == operation)
    );
    let instruction = (
        OperationId::new(),
        WorkflowCommand::Instruct {
            recipient: Recipient::Implementer,
            body: "Add tests".into(),
        },
    );
    let panel = state.workflows.get_mut(&session).unwrap();
    panel.pending = Some(instruction.clone());
    panel.draft.replace("Add tests");
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: WorkflowJob {
                control: Some(instruction),
                ..job
            },
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: Some(fixture_run(session)),
                pending_start: None,
            })),
        }),
    );
    let panel = state.workflow_panel(session).unwrap();
    assert!(panel.pending.is_none());
    assert!(panel.draft.value().is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One correlated retry scenario keeps operation/draft assertions together.
fn workflow_control_roundtrip_preserves_unknown_requests_and_newer_text() {
    use crate::usecase::application::workflow::{
        WorkflowEdit, WorkflowError, WorkflowJob, fixture_run,
    };
    use usagi_core::domain::workflow::{WorkflowCommand, WorkflowSnapshot};
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let effects = submit_closeup_workflow(&mut state, session, "");
    assert_eq!(effects.len(), 2);
    let job = WorkflowJob {
        workspace,
        session,
        control: None,
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: None,
                pending_start: None,
            })),
        }),
    );
    assert!(
        update(
            &mut state,
            AppEvent::WorkflowInput {
                session,
                key: AppKey::SaveRoles
            }
        )
        .is_empty()
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::Paste("Build login".into()),
        },
    );
    let first = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let Effect::Workflow(submit) = first[0].clone() else {
        panic!("expected workflow effect")
    };
    assert!(
        matches!(&submit.control, Some((_, WorkflowCommand::Start { goal, .. })) if goal == "Build login")
    );
    assert!(
        update(
            &mut state,
            AppEvent::WorkflowInput {
                session,
                key: AppKey::SaveRoles
            }
        )
        .is_empty()
    );
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit.clone(),
            result: Err(WorkflowError {
                message: "lost response".into(),
                unconfirmed: true,
            }),
        }),
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::Paste(" plus tests".into()),
        },
    );
    assert_eq!(
        update(
            &mut state,
            AppEvent::WorkflowInput {
                session,
                key: AppKey::SaveRoles
            }
        ),
        first
    );
    let run = fixture_run(session);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit,
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: Some(run.clone()),
                pending_start: None,
            })),
        }),
    );
    assert_eq!(
        state.workflow_panel(session).unwrap().draft.value(),
        "Build login plus tests"
    );
    assert!(state.workflow_panel(session).unwrap().pending.is_none());
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Start,
        },
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Delete,
        },
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::End,
        },
    );
    for key in [
        AppKey::Left,
        AppKey::Right,
        AppKey::Up,
        AppKey::Down,
        AppKey::Tab,
        AppKey::PageUp,
        AppKey::PageDown,
        AppKey::Backspace,
        AppKey::Enter,
        AppKey::Escape,
    ] {
        let _ = update(&mut state, AppEvent::WorkflowInput { session, key });
    }
    let next = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let Effect::Workflow(submit) = next[0].clone() else {
        panic!("expected instruction")
    };
    assert!(matches!(
        &submit.control,
        Some((_, WorkflowCommand::Instruct { .. }))
    ));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit.clone(),
            result: Err(WorkflowError {
                message: "invalid".into(),
                unconfirmed: false,
            }),
        }),
    );
    assert!(state.workflow_panel(session).unwrap().pending.is_none());
    // A late reply for the rejected operation cannot clear a new draft.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: submit,
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: Some(run),
                pending_start: None,
            })),
        }),
    );
    assert!(
        !state
            .workflow_panel(session)
            .unwrap()
            .draft
            .value()
            .is_empty()
    );
    // Periodic observation is non-blocking and does not start work.
    state.mascot_tick = 9;
    assert_eq!(
        update(&mut state, AppEvent::Tick),
        vec![Effect::Workflow(job)]
    );
}

#[test]
fn workflow_rejects_foreign_stale_and_overlay_input() {
    use crate::usecase::application::workflow::{WorkflowEdit, WorkflowJob};
    use usagi_core::domain::workflow::WorkflowSnapshot;
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    let job = WorkflowJob {
        workspace,
        session,
        control: None,
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session,
                run: None,
                pending_start: None,
            })),
        }),
    );
    assert!(state.workflow_panel(session).is_none());
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Delete,
        },
    );
    assert!(submit_closeup_workflow(&mut state, session, "invalid").is_empty());
    let _ = submit_closeup_workflow(&mut state, session, "");
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Workflow {
            job: job.clone(),
            result: Ok(Box::new(WorkflowSnapshot {
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                session: SessionId::new(),
                run: None,
                pending_start: None,
            })),
        }),
    );
    assert!(state.workflow_panel(session).unwrap().error.is_some());
    let foreign = WorkflowJob {
        workspace: WorkspaceId::new(),
        ..job
    };
    assert!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::Workflow {
                job: foreign,
                result: Ok(Box::new(WorkflowSnapshot {
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    session,
                    run: None,
                    pending_start: None
                }))
            })
        )
        .is_empty()
    );
    state.overlay = Some(Overlay::Overview);
    let _ = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::Char('x'),
        },
    );
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: WorkflowEdit::Delete,
        },
    );
    assert!(
        state
            .workflow_panel(session)
            .unwrap()
            .draft
            .value()
            .is_empty()
    );
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
struct FakeBackend {
    effects: Vec<Effect>,
    events: VecDeque<BackendEvent>,
}

#[cfg(test)]
impl FakeBackend {
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
impl BackendPort for FakeBackend {
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
fn pointer_click_resolves_and_selects_each_sidebar_row() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = sized_home(workspace, vec![session], 100, 30);

    // Content begins after the two chrome rows: the session's three lines
    // (summary, change history, Agents — rows 2-4), then the action row
    // (row 5). There is no `main` row or root divider. Each click moves the
    // navigation cursor to that row.
    assert_eq!(
        click_at(&mut state, 5, 2, 0),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(
        click_at(&mut state, 5, 3, 1_000),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(
        click_at(&mut state, 5, 4, 2_000),
        Selection::Target(Target::Session(session))
    );
    assert_eq!(click_at(&mut state, 5, 5, 3_000), Selection::NewSession);

    // A click below every rendered row selects nothing new: the cursor stays
    // where it last landed.
    let before = state.selected();
    let effects = update(
        &mut state,
        AppEvent::Pointer {
            column: 5,
            row: 8,
            at: std::time::Duration::from_millis(4_000),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.selected(), before);
    let _ = click_at(&mut state, 5, 9, 5_000);
    assert_eq!(state.selected(), before);
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
fn sidebar_reserved_pr_cells_without_a_visible_pr_do_not_open_a_modal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let mut dismissed = pr_link(41);
    dismissed.state = PrState::Dismissed;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![dismissed],
        }),
    );

    assert!(
        update(
            &mut state,
            AppEvent::Pointer {
                column: 35,
                row: 3,
                at: std::time::Duration::ZERO,
            },
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), None);
}

#[test]
fn pointer_click_outside_the_sidebar_body_is_inert() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    // A pointer before the first resize has no geometry and cannot resolve.
    let mut ungeometried = AppState::home(workspace, vec![session]);
    assert!(
        update(
            &mut ungeometried,
            AppEvent::Pointer {
                column: 5,
                row: 2,
                at: std::time::Duration::ZERO,
            },
        )
        .is_empty()
    );
    assert_eq!(
        ungeometried.selected(),
        Selection::Target(Target::Session(session))
    );

    let mut state = sized_home(workspace, vec![session], 100, 30);
    let resting = Selection::Target(Target::Session(session));
    for (column, row) in [
        (90, 4), // right-pane column
        (5, 0),  // header row
        (5, 1),  // spacer row
    ] {
        let _ = click_at(&mut state, column, row, u64::from(row));
        assert_eq!(state.selected(), resting);
    }
    // Zero dimensions fall back to 80x24, so a mid-sidebar click still lands.
    let mut zeroed = sized_home(workspace, vec![session], 0, 0);
    assert_eq!(click_at(&mut zeroed, 5, 2, 0), resting);
    // A viewport at or under the chrome, and a click past the content
    // capacity, both resolve to nothing.
    let tiny = sized_home(workspace, vec![session], 100, 2);
    assert!(tiny.sidebar_selection_at(5, 2).is_none());
    let short = sized_home(workspace, vec![SessionId::new()], 100, 8);
    assert!(short.sidebar_selection_at(5, 7).is_none());
}

#[test]
fn pointer_click_handles_single_body_line_and_overflow() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    // body_height == 1 (height 3): only the first row is addressable.
    let single = sized_home(workspace, vec![session], 100, 3);
    assert_eq!(
        single.sidebar_selection_at(5, 2),
        Some(Selection::Target(Target::Session(session)))
    );
    assert_eq!(single.sidebar_selection_at(5, 5), None);
    // A click past the single addressable body line resolves to nothing.
    let overflow = sized_home(workspace, vec![session], 100, 3);
    assert_eq!(overflow.sidebar_selection_at(5, 4), None);
}

#[test]
fn pointer_click_reaches_the_scrolled_viewport_tail() {
    let workspace = WorkspaceId::new();
    let sessions: Vec<SessionId> = (0..6).map(|_| SessionId::new()).collect();
    // A short viewport cannot show every row at once. Moving the cursor to the
    // tail (`+ new session`) scrolls the list, and a click on the last body row
    // still resolves to the row the frame now shows there.
    let mut state = sized_home(workspace, sessions.clone(), 100, 10);
    for _ in 0..sessions.len() {
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    }
    assert_eq!(state.selected(), Selection::NewSession);
    // The short viewport scrolled to the tail. The mascot reserves the sidebar
    // foot, leaving three clickable body rows — not enough for a session's
    // three lines plus the action, so the frame shows the action alone on the
    // first of them (row 2).
    let hit = state
        .sidebar_selection_at(5, 2)
        .expect("the tail row is addressable once scrolled");
    assert_eq!(hit, Selection::NewSession);
    let _ = click_at(&mut state, 5, 2, 0);
    assert_eq!(state.selected(), Selection::NewSession);
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

    let mut scrolled = sized_home(workspace, sessions.clone(), 100, 14);
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
fn consumed_double_click_does_not_turn_a_third_press_into_activation() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let _ = click_at(&mut state, 5, 2, 1_000);
    let _ = click_at(&mut state, 5, 2, 1_100);
    assert_eq!(state.active(), Some(session));
    state.route = Route::Home(HomeMode::Switch);
    let _ = click_at(&mut state, 5, 2, 1_200);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
}

#[test]
fn pointer_click_is_inert_while_an_overlay_owns_the_surface() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    // Open the workspace Overview overlay, then click a background session row.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    let before = state.selected();
    state.pending_session_click = Some((session, std::time::Duration::from_millis(1_000)));
    let effects = update(
        &mut state,
        AppEvent::Pointer {
            column: 5,
            row: 2,
            at: std::time::Duration::from_millis(1_100),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.selected(), before);
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.pending_session_click.is_none());
    state.overlay = None;
    let _ = click_at(&mut state, 5, 2, 1_200);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));

    // The inline create form owns the same background pointer boundary.
    state.overlay = Some(Overlay::CreateSession);
    let _ = update(
        &mut state,
        AppEvent::Pointer {
            column: 5,
            row: 2,
            at: std::time::Duration::from_millis(1_300),
        },
    );
    assert!(state.pending_session_click.is_none());
    state.overlay = None;
    let _ = click_at(&mut state, 5, 2, 1_400);
    assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
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
fn tick_advances_only_the_mascot_animation_frame() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.mascot_tick(), 0);
    let _ = update(&mut state, AppEvent::Tick);
    assert_eq!(state.mascot_tick(), 1);
}

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
    let workspace = WorkspaceId::new();
    let run = SupervisorRunId::new();
    let mut state = AppState::home(workspace, Vec::new());
    state.set_work_mode(WorkMode::GoalDriven);
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenDirectorWorkRuns)).is_empty());
    assert!(state.director_drawer_open());
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenDirectorRunOverview(run)),
    );
    let _ = update(
        &mut state,
        AppEvent::Key(AppKey::OpenDirectorConsole(
            DirectorConsoleParent::RunOverview(run),
        )),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::DirectorBack));
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));

    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(!state.director_drawer_open());
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert!(!state.director_drawer_open());
}

#[test]
fn director_mode_transition_selects_its_landing_without_resetting_the_same_mode() {
    let workspace = WorkspaceId::new();
    let run = SupervisorRunId::new();
    let mut state = AppState::home(workspace, Vec::new());

    assert_eq!(state.work_mode(), WorkMode::Classic);
    assert_eq!(state.director_route(), DirectorRoute::Organization);
    state.set_work_mode(WorkMode::Classic);
    assert_eq!(state.director_route(), DirectorRoute::Organization);

    state.set_work_mode(WorkMode::GoalDriven);
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorConsole(DirectorConsoleParent::RunOverview(run)),
    ));
    let retained = DirectorRoute::Console(DirectorConsoleParent::RunOverview(run));
    assert_eq!(state.director_route(), retained);

    state.set_work_mode(WorkMode::GoalDriven);
    assert_eq!(
        state.director_route(),
        retained,
        "re-observing the same mode must preserve a retained deep route"
    );

    state.set_work_mode(WorkMode::Classic);
    assert_eq!(state.director_route(), DirectorRoute::Organization);
}

#[test]
fn director_workflows_reject_each_others_routes() {
    let workspace = WorkspaceId::new();
    let run = SupervisorRunId::new();
    let mut state = AppState::home(workspace, Vec::new());

    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorWorkRuns,
    ));
    assert_eq!(state.director_route(), DirectorRoute::Organization);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorRunOverview(run),
    ));
    assert_eq!(state.director_route(), DirectorRoute::Organization);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorConsole(DirectorConsoleParent::RunOverview(run)),
    ));
    assert_eq!(state.director_route(), DirectorRoute::Organization);

    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorConsole(DirectorConsoleParent::Organization),
    ));
    assert_eq!(
        state.director_route(),
        DirectorRoute::Console(DirectorConsoleParent::Organization)
    );

    state.set_work_mode(WorkMode::GoalDriven);
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorOrganization,
    ));
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorConsole(DirectorConsoleParent::Organization),
    ));
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorRunOverview(run),
    ));
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));

    // Defensive normalization also repairs state retained by an older
    // binary or an asynchronous route change that crossed workflows.
    state.director_route = DirectorRoute::Organization;
    director_back(&mut state);
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
}

#[test]
fn director_launch_completion_after_mode_switch_stays_in_the_selected_workflow() {
    let workspace = WorkspaceId::new();
    let run = SupervisorRunId::new();

    // A Goal launch may finish after the operator has returned to classic.
    // The Run remains daemon-owned, but the classic tree stays visible.
    let mut goal_state = AppState::home(workspace, Vec::new());
    goal_state.set_work_mode(WorkMode::GoalDriven);
    let goal_operation = OperationId::new();
    goal_state.director_launching = Some(goal_operation);
    goal_state.set_work_mode(WorkMode::Classic);
    assert_eq!(goal_state.director_launching(), Some(goal_operation));
    let _ = update(
        &mut goal_state,
        AppEvent::DirectorLaunchFinished {
            operation: goal_operation,
            supervisor_run_id: Some(run),
            succeeded: true,
        },
    );
    assert_eq!(goal_state.director_route(), DirectorRoute::Organization);

    // Conversely, a classic launch completion must not expose Organization
    // after the operator moved to the goal-driven tree.
    let mut classic_state = AppState::home(workspace, Vec::new());
    let classic_operation = OperationId::new();
    classic_state.director_launching = Some(classic_operation);
    classic_state.set_work_mode(WorkMode::GoalDriven);
    assert_eq!(classic_state.director_launching(), Some(classic_operation));
    let _ = update(
        &mut classic_state,
        AppEvent::DirectorLaunchFinished {
            operation: classic_operation,
            supervisor_run_id: None,
            succeeded: true,
        },
    );
    assert_eq!(classic_state.director_route(), DirectorRoute::WorkRuns);
}

#[test]
fn director_route_commands_are_guarded_and_open_from_the_root_shell() {
    let workspace = WorkspaceId::new();
    let run = SupervisorRunId::new();
    let mut state = AppState::home(workspace, Vec::new());

    state.director_goal = "discard me".into();
    state.director_new = DirectorNew::Empty;
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorOrganization
    ));
    assert_eq!(state.director_route(), DirectorRoute::Organization);
    assert_eq!(state.director_new(), DirectorNew::Idle);
    assert!(state.director_goal().is_empty());

    // Classic consumes the Work Runs command without crossing workflows.
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorWorkRuns
    ));
    assert_eq!(state.director_route(), DirectorRoute::Organization);

    state.set_work_mode(WorkMode::GoalDriven);
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorOrganization
    ));
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);

    state.director_launching = Some(OperationId::new());
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorWorkRuns
    ));
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    state.director_route = DirectorRoute::RunOverview(run);
    assert!(update_director_route_key(&mut state, &AppKey::DirectorBack));
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));

    state.director_launching = None;
    state.director_new = DirectorNew::Empty;
    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorWorkRuns
    ));
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));

    state.director_goal = "cancel me".into();
    director_back(&mut state);
    assert_eq!(state.director_new(), DirectorNew::Idle);
    assert!(state.director_goal().is_empty());

    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    assert!(state.root_terminal_drawer_open());
    state.director_launching = Some(OperationId::new());
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorWorkRuns));
    assert!(!state.director_drawer_open());
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    state.director_launching = None;
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorWorkRuns));
    assert!(state.director_drawer_open());
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    state.director_goal = "clear on route".into();
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorWorkRuns));
    assert!(state.director_drawer_open());
    assert_eq!(state.director_route(), DirectorRoute::WorkRuns);
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
    assert!(state.director_goal().is_empty());

    assert!(update_director_route_key(
        &mut state,
        &AppKey::OpenDirectorRunOverview(run)
    ));
    assert_eq!(state.director_route(), DirectorRoute::RunOverview(run));

    state.set_work_mode(WorkMode::Classic);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorWorkRuns));
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
    assert_eq!(state.director_route(), DirectorRoute::Organization);
}

#[test]
fn director_new_picker_has_deterministic_candidates_and_cancel() {
    let workspace = WorkspaceId::new();
    let mut state = AppState::home(workspace, Vec::new());
    state.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::SakanaAi]),
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
        DirectorNew::Choosing(DefaultModel::SakanaAi)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::Claude)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::SakanaAi)
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
fn classic_remains_the_default_director_launch() {
    let workspace = WorkspaceId::new();
    let mut state = sized_home(workspace, Vec::new(), 100, 30);
    assert_eq!(state.work_mode(), WorkMode::Classic);
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
        panic!("classic launch must emit one root Agent effect");
    };
    let _ = update(
        &mut state,
        AppEvent::DirectorLaunchFinished {
            operation: *operation_id,
            supervisor_run_id: None,
            succeeded: false,
        },
    );
    assert_eq!(state.director_route(), DirectorRoute::Organization);
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
fn pointer_focus_moves_only_to_an_open_workspace_drawer() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    assert!(
        update(
            &mut state,
            AppEvent::WorkspaceDrawerFocused(WorkspaceDrawerFocus::Director),
        )
        .is_empty()
    );
    assert_eq!(state.workspace_drawer_focus(), None);

    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(state.root_terminal_drawer_open());
    assert!(state.director_drawer_open());
    let _ = update(
        &mut state,
        AppEvent::WorkspaceDrawerFocused(WorkspaceDrawerFocus::Terminal),
    );
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );

    let _ = update(
        &mut state,
        AppEvent::WorkspaceDrawerFocused(WorkspaceDrawerFocus::Director),
    );
    assert_eq!(
        state.workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Director)
    );
    assert!(!state.root_terminal_full_height());
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

    state.set_agent_models(AvailableModels::all(), DefaultModel::SakanaAi);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew));
    assert_eq!(
        state.director_new(),
        DirectorNew::Choosing(DefaultModel::SakanaAi)
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
    state.set_agent_models(
        AvailableModels::new([DefaultModel::SakanaAi]),
        DefaultModel::SakanaAi,
    );
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
    assert_eq!(
        profile.as_ref().map(AgentProfileId::as_str),
        Some("sakana-ai")
    );
    assert_eq!(state.director_launching(), Some(*operation_id));

    // Reopening New, double Enter, and a stale completion cannot cross the
    // operation fence.
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenDirectorNew)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
    let _ = update(
        &mut state,
        AppEvent::DirectorLaunchFinished {
            operation: OperationId::new(),
            supervisor_run_id: None,
            succeeded: true,
        },
    );
    assert_eq!(state.director_launching(), Some(*operation_id));
    let _ = update(
        &mut state,
        AppEvent::DirectorLaunchFinished {
            operation: *operation_id,
            supervisor_run_id: None,
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
fn director_picker_maps_each_cli_fixture_to_one_explicit_profile() {
    let workspace = WorkspaceId::new();
    for (model, expected) in [
        (DefaultModel::Claude, "claude"),
        (DefaultModel::OpenAi, "codex"),
        (DefaultModel::SakanaAi, "sakana-ai"),
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
                state.pr_overlay = Some(PrOverlay::loading(Target::Session(first)));
            }
            Overlay::Preview => {
                state.preview_overlay = Some(PreviewOverlay::loading(Target::Session(first)));
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
fn cursor_moves_without_changing_active_target() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
    assert_eq!(state.active(), Some(first));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.active(), Some(second));
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
fn fake_backend_records_effects_and_replays_events() {
    let (workspace, first, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    let mut backend = FakeBackend::default();
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
        (first_b.clone(), AgentPhase::Waiting),
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

#[test]
fn switch_ctrl_x_removes_safely_and_plain_x_is_inert() {
    let (workspace, first, second) = ids();
    let mut state = AppState::home(workspace, vec![first, second]);

    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::RemoveSession {
            workspace,
            session: first,
            force: false,
            force_delete_branch: false,
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
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        }]
    );
    assert_eq!(state.selected(), Selection::Target(Target::Session(second)));
}

#[test]
fn switch_ctrl_x_safely_removes_regular_sessions_and_purges_integrity_orphans() {
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
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        }],
        "an available session stays on the safe removal path"
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
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        }],
        "an ordinary delete failure keeps the safe removal path"
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
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        }],
        "an integrity stage without the failed lifecycle stays safe"
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
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        }]
    );
}

// Force removal is never a single unmodified letter. Ctrl-X retries on the
// safe path; Enter on the failed row opens the explicit force confirmation.
#[test]
fn failed_delete_ctrl_x_stays_safe_and_plain_x_is_inert() {
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
            force: false,
            force_delete_branch: false,
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
fn fake_port_keeps_note_and_environment_edits_on_safe_failures() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let mut backend = FakeBackend::default();

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

#[test]
fn garden_shortcut_opens_without_replacing_a_front_surface() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);

    state.notice = Some(Notice::new("stale feedback"));
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenGarden)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(state.notice().is_none());

    state.overlay = None;
    let _ = update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));
    assert_eq!(state.overlay(), None);
    assert!(update(&mut state, AppEvent::Key(AppKey::OpenGarden)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));

    for overlay in [Overlay::Overview, Overlay::Closeup] {
        state.overlay = Some(overlay);
        assert!(update(&mut state, AppEvent::Key(AppKey::OpenGarden)).is_empty());
        assert_eq!(state.overlay(), Some(overlay));
    }
}

#[test]
fn overview_garden_opens_a_screen_saver_that_any_key_wakes() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Overview);

    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into()))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(state.notice().is_none());

    // 最初の入力は wake-up として消費され Home へ戻る。Escape 専用ではなく、
    // 矢印や drawer を開く key も背面へ渡らない。
    for key in [
        AppKey::Escape,
        AppKey::Left,
        AppKey::Right,
        AppKey::Down,
        AppKey::ToggleDirectorDrawer,
        AppKey::OpenDirectorNew,
    ] {
        state.overlay = Some(Overlay::Garden);
        assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        assert_eq!(state.overlay(), None);
        assert!(!state.director_drawer_open());
    }

    state.overlay = Some(Overlay::Overview);
    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden extra".into()))
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
fn garden_click_and_arrow_keys_wake_the_overlay() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    state.overlay = Some(Overlay::Garden);

    assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
    assert_eq!(state.overlay(), None);

    state.overlay = Some(Overlay::Garden);
    assert!(update(&mut state, AppEvent::GardenClick(GardenClick::Dismiss)).is_empty());
    assert_eq!(state.overlay(), None);
}

#[test]
fn garden_list_scroll_keeps_home_selection_and_does_not_emit_effects() {
    let (workspace, a, b) = ids();
    let mut state = AppState::home(workspace, vec![a, b]);
    let selected = state.selected();
    let active = state.active();
    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Scroll { offset: 8 })
        )
        .is_empty()
    );
    assert_eq!(state.garden_sidebar_scroll(), 0);
    let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Scroll { offset: 8 })
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert_eq!(state.garden_sidebar_scroll(), 8);
    assert_eq!(state.selected(), selected);
    assert_eq!(state.active(), active);
    assert!(update(&mut state, AppEvent::GardenClick(GardenClick::Dismiss)).is_empty());
    assert_eq!(state.overlay(), None);
    assert_eq!(state.garden_sidebar_scroll(), 0);
}

#[test]
fn manual_garden_refuses_an_unavailable_layout_without_leaving_an_overlay() {
    let (workspace, _, _) = ids();
    let mut state = AppState::home(workspace, Vec::new());
    assert!(update(&mut state, AppEvent::GardenAvailability(false)).is_empty());
    state.overlay = Some(Overlay::Overview);

    assert!(
        update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into()))
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), None);
    assert!(
        state
            .notice()
            .is_some_and(|notice| { notice.message.as_str().contains("at least 64 columns") })
    );
    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD)).is_empty());
    assert_eq!(state.overlay(), None);

    assert!(update(&mut state, AppEvent::GardenAvailability(true)).is_empty());
    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert!(update(&mut state, AppEvent::GardenAvailability(false)).is_empty());
    assert_eq!(state.overlay(), None);
}

/// Just under the threshold nothing happens; reaching it opens the garden.
/// The reducer owns no clock, so the whole timer is one injected duration.
#[test]
fn the_garden_opens_only_once_home_reaches_the_idle_threshold() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);

    let almost = GARDEN_IDLE_THRESHOLD
        .checked_sub(std::time::Duration::from_millis(1))
        .expect("the threshold is longer than a millisecond");
    assert!(update(&mut state, AppEvent::IdleElapsed(almost)).is_empty());
    assert_eq!(state.overlay(), None);

    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));

    // Idle keeps being reported while the garden is up; that is inert rather
    // than a second open, and the route underneath is untouched.
    assert!(update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD * 4)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
}

/// A Closeup with no overlay in front of it is eligible — including one
/// attached to a live terminal, which keeps running behind the garden.
#[test]
fn an_idle_closeup_is_still_eligible_for_the_garden() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    state.overlay = None;

    let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(state.overlay(), Some(Overlay::Garden));
    // The garden is a layer: the route and active target behind it are the
    // ones the wake-up returns to.
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(state.active(), Some(session));
}

/// Confirmations, form drafts, read-only surfaces, and the Director drawer
/// all stay in front: an unsent edit or a destructive prompt must never be
/// covered by a screen saver.
#[test]
fn a_front_surface_keeps_the_idle_garden_away() {
    let (workspace, session, _) = ids();
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
        Overlay::Prs,
        Overlay::Preview,
        Overlay::CreateSessionError,
        Overlay::Garden,
    ] {
        let mut state = sized_home(workspace, vec![session], 100, 30);
        state.overlay = Some(overlay);
        let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        assert_eq!(
            state.overlay(),
            Some(overlay),
            "{overlay:?} was replaced by the garden"
        );
    }

    let mut state = sized_home(workspace, vec![session], 100, 30);
    let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
    assert!(state.director_drawer_open());
    let _ = update(&mut state, AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(state.overlay(), None);
    assert!(state.director_drawer_open());
}

/// Resize arrives as a per-frame level, so only its edge closes the garden.
#[test]
fn a_resize_closes_the_garden_but_a_resampled_size_does_not() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    state.overlay = Some(Overlay::Garden);

    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 100,
            height: 30,
        },
    );
    assert_eq!(state.overlay(), Some(Overlay::Garden));

    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 120,
            height: 30,
        },
    );
    assert_eq!(state.overlay(), None);

    // A resize with no garden up is the plain size update it always was.
    let _ = update(
        &mut state,
        AppEvent::Resize {
            width: 80,
            height: 24,
        },
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.size, Some((80, 24)));
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

/// Everything else in the garden is a wake-up: consume the press, restore
/// the Home from before the screen saver, and change no target.
#[test]
fn clicking_beside_the_usagi_only_returns_home() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 100, 30);
    let (selected, active) = (state.selected(), state.active());
    state.overlay = Some(Overlay::Garden);

    assert!(update(&mut state, AppEvent::GardenClick(GardenClick::Dismiss)).is_empty());
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), selected);
    assert_eq!(state.active(), active);
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
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
fn unavailable_garden_closes_with_visible_feedback() {
    let (workspace, first, second) = ids();
    let mut state = sized_home(workspace, vec![first, second], 40, 10);
    state.overlay = Some(Overlay::Garden);

    assert!(update(&mut state, AppEvent::GardenUnavailable).is_empty());
    assert_eq!(state.overlay(), None);
    assert!(
        state
            .notice()
            .is_some_and(|notice| notice.message.contains("terminal size"))
    );
}

/// The press and the snapshot race. A session that left the workspace
/// between the frame and the click is a stale target: close the garden, run
/// nothing.
#[test]
fn a_click_on_a_vanished_usagi_only_closes_the_garden() {
    let (workspace, session, gone) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let (selected, active) = (state.selected(), state.active());
    state.overlay = Some(Overlay::Garden);

    assert!(
        update(
            &mut state,
            AppEvent::GardenClick(GardenClick::Visit {
                workspace,
                session: gone,
                agent: None,
            })
        )
        .is_empty()
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), selected);
    assert_eq!(state.active(), active);
    assert_eq!(state.route(), Route::Home(HomeMode::Switch));
}

/// A click resolved against a frame the garden has since left behind must
/// not activate anything.
#[test]
fn a_garden_click_without_an_open_garden_is_inert() {
    let (workspace, session, _) = ids();
    let mut state = sized_home(workspace, vec![session], 100, 30);
    let route = state.route();

    for click in [
        GardenClick::Visit {
            workspace,
            session,
            agent: None,
        },
        GardenClick::Dismiss,
    ] {
        assert!(update(&mut state, AppEvent::GardenClick(click)).is_empty());
        assert_eq!(state.overlay(), None);
        assert_eq!(state.route(), route);
    }
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

fn safe_error(message: &str) -> SafeError {
    SafeError {
        message: SafeMessage::new(message),
        error_id: "overlay".into(),
    }
}

#[test]
fn pr_overlay_opens_reflows_material_navigates_opens_and_closes() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);

    // `p` requests the active target's list without showing an empty modal.
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::OpenPrs)),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().unwrap().prs().is_empty());

    // A list for another target is ignored; the matching one fills the overlay.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target: Target::Root(workspace),
            revision: 0,
            prs: vec![pr_link(9)],
        }),
    );
    assert!(state.pr_overlay().unwrap().prs().is_empty());
    let prs = vec![pr_link(1), pr_link(2)];
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: prs.clone(),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 2);
    assert_eq!(state.pr_overlay().unwrap().selected(), 0);
    assert_eq!(state.session_prs(session), Some(prs.as_slice()));

    // A delayed older snapshot cannot roll either the modal or sidebar
    // projection back after a newer daemon revision has landed.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: vec![pr_link(99)],
        }),
    );
    assert_eq!(state.pr_overlay().unwrap().prs(), prs.as_slice());
    assert_eq!(state.session_prs(session), Some(prs.as_slice()));

    // Down/Up wrap around the list.
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.pr_overlay().unwrap().selected(), 1);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    assert_eq!(state.pr_overlay().unwrap().selected(), 0);
    let _ = update(&mut state, AppEvent::Key(AppKey::Up));
    assert_eq!(state.pr_overlay().unwrap().selected(), 1);

    // Enter opens the selected PR through the browser effect.
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Enter)),
        vec![Effect::OpenPullRequest {
            url: prs[1].url().to_owned(),
        }]
    );

    // Esc closes the overlay and discards its state.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());

    // Reopening from the cached revision is immediate. A duplicate response
    // cannot make the modal diverge from that sidebar projection.
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::OpenPrs)),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.pr_overlay().unwrap().prs(), prs.as_slice());
    let revision = state.session_pr_revision();
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(55)],
        }),
    );
    assert_eq!(state.pr_overlay().unwrap().prs(), prs.as_slice());
    assert_eq!(state.session_pr_revision(), revision);

    // Removing the stable session identity also removes its cached PR rows;
    // a later session reusing display text cannot inherit the badge.
    let revision = state.session_pr_revision();
    assert_eq!(
        update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(Vec::new()))
        ),
        vec![Effect::SyncPullRequestTargets {
            sessions: Vec::new()
        }]
    );
    assert!(state.session_prs(session).is_none());
    assert!(state.session_pr_revision() > revision);
    assert!(state.pr_overlay().is_none());
    assert_eq!(state.overlay(), None);
    assert_eq!(state.selected(), Selection::Idle);
    assert_eq!(state.active(), None);
}

#[test]
fn pr_overlay_stays_hidden_without_prs_and_reports_loading_errors() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), None);
    // A fetch error is not an authoritative empty snapshot, so its safe
    // diagnostic remains visible in the PR modal.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsError {
            target,
            error: safe_error("gh unavailable"),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(
        state
            .pr_overlay()
            .unwrap()
            .error()
            .map(|error| error.message.as_str()),
        Some("gh unavailable")
    );

    // An authoritative empty result discards the pending modal state.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());

    // Reopening from the known-empty cache also stays hidden, and the
    // duplicate revision returned by an explicit refresh clears its pending
    // request instead of leaving it to misclassify a future discovery.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_some());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn root_pr_overlay_uses_the_unfiltered_inventory_to_decide_visibility() {
    let (workspace, _, _) = ids();
    let target = Target::Root(workspace);
    let mut state = AppState::home(workspace, Vec::new());
    state.pr_overlay = Some(PrOverlay::loading(target));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(1)],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn delayed_pr_request_does_not_steal_focus_and_empty_refresh_closes_modal() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);

    // A foreground interaction opened after `p` wins over a delayed error.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsError {
            target,
            error: safe_error("gh unavailable"),
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.pr_overlay().is_none());

    // The same foreground interaction also wins over a delayed successful
    // response, while the authoritative PR cache still advances.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let pr = pr_link(41);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    assert!(state.pr_overlay().is_none());
    assert_eq!(state.session_prs(session), Some(std::slice::from_ref(&pr)));

    // A cached PR opens immediately. If a newer authoritative snapshot no
    // longer contains any visible PR, the stale modal closes.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
}

#[test]
fn newly_detected_pr_auto_opens_without_reopening_or_stealing_focus() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);

    // The empty baseline is not actionable. Its first newly discovered URL
    // opens the modal directly from the resident snapshot lane.
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: Vec::new(),
        }),
    );
    assert_eq!(state.overlay(), None);
    let first = pr_link(41);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![first.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().target(), target);
    assert_eq!(
        state.pr_overlay().unwrap().prs(),
        std::slice::from_ref(&first)
    );

    // Closing acknowledges the discovery. A title/state refresh and a
    // duplicate cannot reopen it because neither introduces a new URL.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let mut enriched = first.clone();
    enriched.title = Some("ready for review".into());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![enriched.clone()],
        }),
    );
    assert_eq!(state.overlay(), None);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![enriched.clone()],
        }),
    );
    assert_eq!(state.overlay(), None);

    // A frontmost interaction is never replaced. The new PR still enters
    // the shared cache and is visible the next time the user opens `p`.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
    let second = pr_link(42);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 3,
            prs: vec![enriched, second.clone()],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Overview));
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert!(
        state
            .pr_overlay()
            .unwrap()
            .prs()
            .iter()
            .any(|pr| pr.identity == second.identity)
    );
}

#[test]
fn initial_pr_snapshot_is_a_baseline_and_detected_pr_is_selected() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let existing = (1..=7).map(pr_link).collect::<Vec<_>>();

    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: existing.clone(),
        }),
    );
    assert_eq!(state.overlay(), None);

    let detected = pr_link(8);
    let mut updated = existing;
    updated.push(detected.clone());
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: updated,
        }),
    );

    let overlay = state.pr_overlay().unwrap();
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(overlay.selected(), 7);
    assert_eq!(overlay.selected_pr(), Some(&detected));
}

#[test]
fn pr_reference_filter_copy_dismiss_and_safe_auto_open_modes() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: Vec::new(),
        }),
    );
    let mut reference = pr_link(1);
    reference.auto_open = false;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![reference.clone()],
        }),
    );
    assert_eq!(state.overlay(), None);

    state.route = Route::Home(HomeMode::Closeup);
    let open = pr_link(2);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![reference.clone(), open.clone()],
        }),
    );
    assert_eq!(
        state.overlay(),
        None,
        "switch-only must not steal live input"
    );
    let mut merged = open;
    merged.state = PrState::Merged;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 3,
            prs: vec![reference, merged],
        }),
    );
    assert!(state.celebrates_pr_merge(session));
    for _ in 0..25 {
        let _ = update(&mut state, AppEvent::Tick);
    }
    assert!(!state.celebrates_pr_merge(session));

    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::Char('c'))),
        vec![Effect::CopyPullRequest {
            url: "https://github.com/o/r/pull/1".into()
        }]
    );
    assert!(update(&mut state, AppEvent::Key(AppKey::Char('d'))).is_empty());
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlX)),
        vec![Effect::DismissPullRequest {
            session,
            url: "https://github.com/o/r/pull/1".into()
        }]
    );
}

#[test]
fn open_pr_overlay_tracks_new_detection_and_navigates_status_tabs() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let mut merged = pr_link(2);
    merged.state = PrState::Merged;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![pr_link(1), merged.clone()],
        }),
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    let newly_detected = pr_link(3);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 2,
            prs: vec![pr_link(1), merged, newly_detected.clone()],
        }),
    );
    assert_eq!(
        state.pr_overlay().unwrap().selected_pr(),
        Some(&newly_detected)
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Right));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Open);
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 2);
    for (filter, expected) in [
        (PrFilter::Closed, 0),
        (PrFilter::Merged, 1),
        (PrFilter::All, 3),
    ] {
        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        assert_eq!(state.pr_overlay().unwrap().filter(), filter);
        assert_eq!(state.pr_overlay().unwrap().prs().len(), expected);
    }

    // A resident refresh while the active status tab has no matches keeps
    // the modal open. The unfiltered inventory still has PRs, so the user
    // must be able to navigate to another tab without reopening it.
    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Closed);
    assert!(state.pr_overlay().unwrap().prs().is_empty());
    let mut refreshed_merged = newly_detected.clone();
    refreshed_merged.state = PrState::Merged;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 3,
            prs: vec![pr_link(1), refreshed_merged],
        }),
    );
    assert_eq!(state.overlay(), Some(Overlay::Prs));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Closed);
    assert!(state.pr_overlay().unwrap().prs().is_empty());

    let _ = update(&mut state, AppEvent::Key(AppKey::Left));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Open);
    assert_eq!(state.pr_overlay().unwrap().prs().len(), 1);

    // The old hidden `f` shortcut is inert now that the visible tabs own
    // status navigation.
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('f')));
    assert_eq!(state.pr_overlay().unwrap().filter(), PrFilter::Open);
    assert_eq!(PrFilter::All.label(), "all");
    assert_eq!(PrFilter::Open.label(), "open");
    assert_eq!(PrFilter::Closed.label(), "closed");
    assert_eq!(PrFilter::Merged.label(), "merged");
    assert_eq!(PrFilter::TABS.map(PrFilter::tab_index), [0, 1, 2, 3]);
    assert_eq!(
        PrFilter::TABS.map(PrFilter::previous),
        [
            PrFilter::Merged,
            PrFilter::All,
            PrFilter::Open,
            PrFilter::Closed,
        ]
    );

    state.pr_overlay = None;
    assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
}

#[test]
fn explicit_pr_auto_open_modes_cover_always_notify_and_never() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    for (mode, opens, notifies) in [
        (PrAutoOpen::Always, true, false),
        (PrAutoOpen::NotifyOnly, false, true),
        (PrAutoOpen::Never, false, false),
    ] {
        let mut state = AppState::home(workspace, vec![session]);
        state.route = Route::Home(HomeMode::Closeup);
        state.set_pr_auto_open(mode);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target,
                revision: 0,
                prs: Vec::new(),
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target,
                revision: 1,
                prs: vec![pr_link(7)],
            }),
        );
        assert_eq!(state.overlay() == Some(Overlay::Prs), opens);
        assert_eq!(state.notice().is_some(), notifies);
    }
}

#[test]
fn dismissed_pr_does_not_auto_open() {
    let (workspace, session, _) = ids();
    let target = Target::Session(session);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 0,
            prs: Vec::new(),
        }),
    );
    let mut dismissed = pr_link(41);
    dismissed.state = PrState::Dismissed;
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PullRequestsLoaded {
            target,
            revision: 1,
            prs: vec![dismissed],
        }),
    );

    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_none());
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
    assert!(state.preview_overlay().unwrap().visible_files().is_empty());
    let event = preview_loaded(
        &state,
        target,
        None,
        &["src/lib.rs", "README.md", "src/runtime.rs", "bad\npath"],
        &[],
    );
    let _ = update(&mut state, event);
    assert!(!state.preview_overlay().unwrap().is_loading());
    assert_eq!(state.preview_overlay().unwrap().visible_files().len(), 3);

    // Finder navigation saturates, and filtering resets selection. A query
    // with several matches also exercises fuzzy rank ordering.
    let _ = update(&mut state, AppEvent::Key(AppKey::Char('s')));
    assert_eq!(state.preview_overlay().unwrap().visible_files().len(), 2);
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
fn opening_one_overlay_discards_the_other_state() {
    let (workspace, session, _) = ids();
    let mut state = AppState::home(workspace, vec![session]);
    // Open PRs, dismiss, then open preview: the PR state must not linger.
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert!(state.pr_overlay().is_some());
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
    assert_eq!(state.overlay(), Some(Overlay::Preview));
    assert!(state.pr_overlay().is_none());
    assert!(state.preview_overlay().is_some());
    // And the reverse: opening PRs discards the preview state.
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(state.overlay(), None);
    assert!(state.pr_overlay().is_some());
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
    let editor = DecisionEditor::new(decision.clone());
    assert_eq!(editor.selected_option(), 0);
    assert_eq!(editor.freeform(), "");
    assert!(editor.error().is_none());
    let overlay = DecisionOverlayState {
        selected: 0,
        editor: Some(editor),
    };
    assert_eq!(overlay.selected(), 0);

    let prs = PrOverlay::loading(root);
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
    assert!(update_prs_overlay(&mut state, &AppKey::Enter).is_empty());
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
    state.decisions = vec![decision.clone()];
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
    state.pr_overlay = Some(PrOverlay::loading(Target::Session(session)));
    for key in [AppKey::Up, AppKey::Down, AppKey::Home] {
        let _ = update_prs_overlay(&mut state, &key);
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

#[test]
fn workflow_uses_saved_agents_without_overwriting_edits_and_submits_exact_choices() {
    use super::super::workflow::WorkflowJob;
    use usagi_core::domain::{
        settings::DefaultModel,
        workflow::{WorkflowAgents, WorkflowCommand, WorkflowSnapshot},
    };
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    state.active = Some(session);
    state.route = Route::Home(HomeMode::Closeup);
    submit_closeup_workflow(&mut state, session, "");
    let saved = WorkflowAgents {
        planner: DefaultModel::Agy,
        implementer: DefaultModel::Claude,
        reviewer: DefaultModel::OpenAi,
    };
    let snapshot = AppEvent::Backend(BackendEvent::Workflow {
        job: WorkflowJob {
            workspace,
            session,
            control: None,
        },
        result: Ok(Box::new(WorkflowSnapshot {
            agents: saved,
            session,
            run: None,
            pending_start: None,
        })),
    });
    let _ = update(&mut state, snapshot.clone());
    assert_eq!(state.workflow_panel(session).unwrap().agents, saved);
    for key in [
        AppKey::Paste("Task".into()),
        AppKey::Tab,
        AppKey::Right,
        AppKey::Left,
        AppKey::Right,
        AppKey::Char('x'),
    ] {
        let _ = update(&mut state, AppEvent::WorkflowInput { session, key });
    }
    let _ = update(
        &mut state,
        AppEvent::WorkflowEdit {
            session,
            edit: super::super::workflow::WorkflowEdit::Start,
        },
    );
    assert_eq!(state.workflow_panel(session).unwrap().draft.cursor(), 4);
    let chosen = state.workflow_panel(session).unwrap().agents;
    assert_ne!(chosen.planner, saved.planner);
    assert_eq!(chosen.implementer, saved.implementer);
    let _ = update(&mut state, snapshot);
    assert_eq!(state.workflow_panel(session).unwrap().agents, chosen);
    assert_eq!(state.workflow_panel(session).unwrap().draft.value(), "Task");
    let effects = update(
        &mut state,
        AppEvent::WorkflowInput {
            session,
            key: AppKey::SaveRoles,
        },
    );
    let Effect::Workflow(job) = &effects[0] else {
        panic!("workflow submission");
    };
    assert!(
        matches!(&job.control, Some((_, WorkflowCommand::Start { goal, agents })) if goal == "Task" && *agents == chosen)
    );
}

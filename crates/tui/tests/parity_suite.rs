//! Deterministic parity scenarios for the TUI reducers and Home frame.
//!
//! This suite deliberately drives only pure reducers and the fake backend seam.
//! It is safe to run in CI without a PTY, daemon socket, clock, or terminal.

use std::collections::VecDeque;
use std::path::PathBuf;

use usagi_core::domain::id::{
    AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, OperationId, SessionId, TerminalId,
    TerminalRef, WorkspaceId, WorktreeId,
};
use usagi_core::domain::session_lifecycle::AgentPhase;
use usagi_tui::presentation::frame::{Frame, FrameRenderer, Span};
use usagi_tui::presentation::views::workspace::{HomeProjection, ProjectedSession, render_home};
use usagi_tui::presentation::widgets::display_width;
use usagi_tui::usecase::application::controller::{
    AppEvent, AppKey, AppState, BackendEvent, Effect, Feedback, Overlay, SafeError, SafeMessage,
    TabDirection, Target, TargetPhase, update,
};
use usagi_tui::usecase::application::lifecycle::{
    DaemonEvent, Effect as LifecycleEffect, Event, Interaction, LifecycleState, Mode, PendingRow,
    SessionRow, Target as LifecycleTarget, update as update_lifecycle,
};
use usagi_tui::usecase::application::pane::{
    LivePane, PaneEffect, PaneEvent, PaneKind, PaneSelection, PaneState, TabSelection, reduce,
};
use usagi_tui::usecase::application::pane_runtime::{
    Geometry, PaneRuntime, TerminalError, TerminalInventory, TerminalPort, TerminalSnapshot,
};

/// Fake lifecycle adapter: records requests and replays daemon events in the
/// explicit order supplied by a scenario.  New parity cases can reuse this
/// without adding IO or timing to their fixture.
#[derive(Debug, Default)]
struct FakeLifecycleBackend {
    effects: Vec<LifecycleEffect>,
    events: VecDeque<DaemonEvent>,
}

impl FakeLifecycleBackend {
    fn scripted(events: impl IntoIterator<Item = DaemonEvent>) -> Self {
        Self {
            effects: Vec::new(),
            events: events.into_iter().collect(),
        }
    }

    fn dispatch_and_replay(&mut self, state: &mut LifecycleState, effects: Vec<LifecycleEffect>) {
        self.effects.extend(effects);
        while let Some(event) = self.events.pop_front() {
            let _ = update_lifecycle(state, Event::Daemon(event));
        }
    }
}

fn terminal(workspace: WorkspaceId, session: SessionId) -> TerminalRef {
    TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
    }
}

fn runtime(workspace: WorkspaceId, session: SessionId) -> AgentRuntimeRef {
    AgentRuntimeRef::new(AgentRuntimeId::new(), terminal(workspace, session), session)
        .expect("fixture terminal belongs to its session")
}

fn session_projection(id: SessionId, label: &str) -> ProjectedSession {
    ProjectedSession {
        id,
        label: label.into(),
        detail: "fixture".into(),
        cwd: PathBuf::from(format!("/work/{label}")),
    }
}

fn strip_ansi(text: &str) -> String {
    let mut output = String::new();
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for final_byte in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&final_byte) {
                break;
            }
        }
    }
    output
}

#[test]
fn scripted_fake_lifecycle() {
    let workspace = WorkspaceId::new();
    let created = SessionId::new();
    let create = OperationId::new();
    let mut state = LifecycleState::new(workspace, Vec::new());
    let mut backend = FakeLifecycleBackend::scripted([
        DaemonEvent::Accepted {
            operation_id: create,
            row: PendingRow::Creating {
                label: "alpha".into(),
            },
        },
        DaemonEvent::Progress {
            operation_id: create,
            revision: 1,
            message: "creating".into(),
        },
        DaemonEvent::Succeeded {
            operation_id: create,
            revision: 2,
            created: Some(SessionRow {
                id: created,
                label: "alpha".into(),
            }),
        },
    ]);

    let effects = update_lifecycle(
        &mut state,
        Event::RequestCreate {
            operation_id: create,
            label: "alpha".into(),
        },
    );
    backend.dispatch_and_replay(&mut state, effects);

    assert_eq!(backend.effects.len(), 1);
    assert_eq!(
        state.sessions(),
        &[SessionRow {
            id: created,
            label: "alpha".into()
        }]
    );
    assert_eq!(
        state.selected(),
        usagi_tui::usecase::application::lifecycle::Selection::Target(LifecycleTarget::Session(
            created
        ))
    );
    assert_eq!(state.active(), LifecycleTarget::Session(created));
    assert_eq!(state.mode(), Mode::Closeup);
    assert!(state.pending().is_empty());

    let remove = OperationId::new();
    let mut backend = FakeLifecycleBackend::scripted([
        DaemonEvent::Accepted {
            operation_id: remove,
            row: PendingRow::Removing {
                row: SessionRow {
                    id: created,
                    label: "alpha".into(),
                },
            },
        },
        DaemonEvent::Failed {
            operation_id: remove,
            revision: 1,
            message: "safe remove failure".into(),
        },
    ]);
    let effects = update_lifecycle(
        &mut state,
        Event::RequestRemove {
            operation_id: remove,
            session: created,
        },
    );
    backend.dispatch_and_replay(&mut state, effects);

    assert_eq!(
        state.sessions(),
        &[SessionRow {
            id: created,
            label: "alpha".into()
        }]
    );
    assert_eq!(state.error(), Some("safe remove failure"));
    assert_eq!(state.interaction_count(), 0);
    let _ = update_lifecycle(&mut state, Event::Interaction(Interaction::RightClick));
    assert_eq!(state.interaction_count(), 1);
}

#[test]
fn pane_completion_background_exit() {
    let workspace = WorkspaceId::new();
    let first = SessionId::new();
    let second = SessionId::new();
    let first_target = Target::Session(first);
    let second_target = Target::Session(second);
    let mut state = PaneState::new(PaneSelection::Target(first_target));
    let operation = OperationId::new();
    assert!(
        reduce(
            &mut state,
            PaneEvent::Request {
                operation,
                target: first_target,
                kind: PaneKind::Agent
            }
        )
        .is_empty()
    );
    assert!(
        reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Target(second_target))
        )
        .is_empty()
    );

    let first_terminal = terminal(workspace, first);
    assert!(
        reduce(
            &mut state,
            PaneEvent::Succeeded {
                operation,
                terminal: first_terminal.clone()
            }
        )
        .is_empty()
    );
    assert_eq!(state.tabs().len(), 1);

    let second_terminal = terminal(workspace, second);
    assert!(
        reduce(
            &mut state,
            PaneEvent::Restore(usagi_tui::usecase::application::pane::LivePane {
                terminal: second_terminal.clone(),
                kind: PaneKind::Terminal
            })
        )
        .is_empty()
    );
    assert!(
        reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(
                usagi_tui::usecase::application::pane::TabSelection::Live(second_terminal.clone())
            ))
        )
        .is_empty()
    );
    assert!(reduce(&mut state, PaneEvent::Exited(first_terminal)).is_empty());
    assert_eq!(state.tabs().len(), 1);
    assert_eq!(
        reduce(&mut state, PaneEvent::Exited(second_terminal)),
        vec![PaneEffect::ReturnToCloseup]
    );
}

#[test]
fn quit_phase_error_redaction() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    let runtime = runtime(workspace, session);
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::RuntimePhase {
            runtime,
            phase: AgentPhase::Waiting,
        }),
    );
    assert_eq!(
        state.phase_for(Target::Session(session)),
        TargetPhase::Waiting
    );

    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
    assert!(update(&mut state, AppEvent::Key(AppKey::Char('n'))).is_empty());
    let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
    assert!(state.ctrl_c_grace());
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
    assert!(!state.ctrl_c_grace());

    let safe = SafeError {
        message: SafeMessage::new("terminal unavailable"),
        error_id: "err-fixture-1".into(),
    };
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::Feedback(Feedback::TerminalError(safe))),
    );
    let projection = HomeProjection::from_state(
        &state,
        "東京",
        "/work/root",
        &[session_projection(session, "開発")],
    );
    let frame = render_home(10, 160, &projection)
        .into_iter()
        .map(|line| strip_ansi(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        frame.contains("terminal error: terminal unavailable (err-fixture-1)"),
        "{frame}"
    );
    assert!(!frame.contains("secret=do-not-render"));
}

#[test]
fn home_frame_golden_covers_ansi_cjk_wide_and_tiny_geometry() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    let projection = HomeProjection::from_state(
        &state,
        "東京",
        "/work/root",
        &[session_projection(session, "開発")],
    );

    let lines = render_home(8, 40, &projection);
    assert!(lines.iter().all(|line| display_width(line) <= 40));
    let actual = lines
        .iter()
        .map(|line| strip_ansi(line).trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(actual, include_str!("fixtures/home_cjk.golden").trim_end());

    let tiny = render_home(1, 1, &projection);
    assert_eq!(tiny.len(), 1);
    assert!(tiny.iter().all(|line| display_width(line) <= 1));

    let mut renderer = FrameRenderer::new();
    let _ = renderer.render(Frame::from_lines(4, 1, ["a界b"]));
    assert_eq!(
        renderer.render(Frame::from_lines(4, 1, ["a語b"])).spans,
        vec![Span {
            row: 0,
            column: 1,
            text: "語".into()
        }]
    );
}

#[test]
fn controller_closeup_prefix_and_tab_gating_match_live_model() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut state = AppState::home(workspace, vec![session]);

    // 1/3: Enter reaches Closeup and a tab-less Closeup owns the action modal.
    let _ = update(&mut state, AppEvent::Key(AppKey::Down));
    let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(
        state.route(),
        usagi_tui::usecase::application::controller::Route::Home(
            usagi_tui::usecase::application::controller::HomeMode::Closeup
        )
    );
    assert_eq!(state.overlay(), Some(Overlay::Closeup));
    let projection = HomeProjection::from_state(
        &state,
        "fixture",
        "/work/root",
        &[session_projection(session, "alpha")],
    );
    assert!(
        render_home(24, 80, &projection)
            .join("\n")
            .contains("Closeup: alpha")
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), Some(Overlay::Closeup));

    // 4: once a pane is available the tab surface is frontmost.
    let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
    assert_eq!(state.overlay(), None);
    let projection = HomeProjection::from_state(
        &state,
        "fixture",
        "/work/root",
        &[session_projection(session, "alpha")],
    );
    assert!(
        !render_home(24, 80, &projection)
            .join("\n")
            .contains("Closeup: alpha")
    );

    // 5/6: the forced action surface and prefix tab selection are independent.
    let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
    assert_eq!(state.overlay(), Some(Overlay::Closeup));
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlN)),
        vec![Effect::SelectTab {
            direction: TabDirection::Next,
        }]
    );
    assert_eq!(
        update(&mut state, AppEvent::Key(AppKey::CtrlP)),
        vec![Effect::SelectTab {
            direction: TabDirection::Previous,
        }]
    );
    let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    assert_eq!(state.overlay(), None);

    // 2: the Switch action clears a forced overlay as well as changing mode.
    assert!(update(&mut state, AppEvent::Key(AppKey::CtrlO)).is_empty());
    assert_eq!(
        state.route(),
        usagi_tui::usecase::application::controller::Route::Home(
            usagi_tui::usecase::application::controller::HomeMode::Switch
        )
    );
    assert_eq!(state.overlay(), None);
}

#[derive(Default)]
struct ResumeFixturePort {
    inventory: Vec<TerminalInventory>,
    attachments: Vec<TerminalRef>,
}

impl TerminalPort for ResumeFixturePort {
    fn inventory(&mut self) -> Result<Vec<TerminalInventory>, TerminalError> {
        Ok(self.inventory.clone())
    }

    fn attach(
        &mut self,
        terminal: &TerminalRef,
        _: Option<u64>,
    ) -> Result<TerminalSnapshot, TerminalError> {
        self.attachments.push(terminal.clone());
        Ok(TerminalSnapshot {
            terminal: terminal.clone(),
            output_offset: 0,
            geometry: Geometry { cols: 80, rows: 24 },
            replay: Vec::new(),
            exited: false,
        })
    }

    fn resync(&mut self, _: &TerminalRef) -> Result<TerminalSnapshot, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    fn input(&mut self, _: &TerminalRef, _: &[u8]) -> Result<(), TerminalError> {
        Ok(())
    }

    fn resize(&mut self, _: &TerminalRef, _: Geometry) -> Result<(), TerminalError> {
        Ok(())
    }

    fn detach(&mut self, _: &TerminalRef) {}
}

#[test]
fn resume_compatibility_fixture_falls_back_for_missing_stale_and_old_state() {
    let workspace = WorkspaceId::new();
    let removed_session = SessionId::new();
    let surviving_session = SessionId::new();

    // A saved target is an identity, not a row index. A refreshed snapshot that
    // no longer contains it migrates both selection and action target to root.
    let mut lifecycle = LifecycleState::new(
        workspace,
        vec![SessionRow {
            id: removed_session,
            label: "old".into(),
        }],
    );
    let create = OperationId::new();
    let _ = update_lifecycle(
        &mut lifecycle,
        Event::RequestCreate {
            operation_id: create,
            label: "select old target".into(),
        },
    );
    let _ = update_lifecycle(
        &mut lifecycle,
        Event::Daemon(DaemonEvent::Accepted {
            operation_id: create,
            row: PendingRow::Creating {
                label: "select old target".into(),
            },
        }),
    );
    let _ = update_lifecycle(
        &mut lifecycle,
        Event::Daemon(DaemonEvent::Succeeded {
            operation_id: create,
            revision: 1,
            created: Some(SessionRow {
                id: removed_session,
                label: "old".into(),
            }),
        }),
    );
    let _ = update_lifecycle(
        &mut lifecycle,
        Event::Snapshot {
            sessions: vec![SessionRow {
                id: surviving_session,
                label: "current".into(),
            }],
        },
    );
    assert_eq!(
        lifecycle.selected(),
        usagi_tui::usecase::application::lifecycle::Selection::Target(LifecycleTarget::Root)
    );
    assert_eq!(lifecycle.active(), LifecycleTarget::Root);

    let saved = terminal(workspace, surviving_session);
    let pane = PaneState::with_live(
        PaneSelection::Tab(TabSelection::Live(saved.clone())),
        vec![LivePane {
            terminal: saved.clone(),
            kind: PaneKind::Terminal,
        }],
    );

    // Missing inventory data is a safe no-attach fallback.
    let mut missing = PaneRuntime::new(pane.clone());
    let mut port = ResumeFixturePort::default();
    missing.reconnect(&mut port);
    assert!(missing.pane().tabs().is_empty());
    assert!(port.attachments.is_empty());

    // An old TerminalRef (same terminal ID but another daemon incarnation) is
    // stale rather than a candidate for heuristic migration.
    let mut old = saved.clone();
    old.daemon_generation = DaemonGeneration::new();
    let mut stale = PaneRuntime::new(pane);
    let mut port = ResumeFixturePort {
        inventory: vec![TerminalInventory {
            terminal: old,
            live: true,
        }],
        ..ResumeFixturePort::default()
    };
    stale.reconnect(&mut port);
    assert!(stale.pane().tabs().is_empty());
    assert!(port.attachments.is_empty());
}

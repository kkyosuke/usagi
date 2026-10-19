use super::*;
use crate::presentation::workspace_runtime::PaneRestoreTarget;
use crate::usecase::application::Key;
use crate::usecase::application::controller::{BackendEvent, Effect, HomeMode, Route};
use crate::usecase::application::interrupted_tab::InterruptedTab;
use crate::usecase::application::pane::{PaneEvent, TabSelection};
use usagi_core::domain::agent::ProviderResumeReason;
use usagi_core::domain::id::{
    AgentContinuationRef, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
    WorktreeId,
};
use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};

fn terminal(workspace: WorkspaceId, session: Option<SessionId>) -> TerminalRef {
    TerminalRef {
        workspace_id: workspace,
        session_id: session,
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        worktree_id: WorktreeId::new(),
    }
}

#[test]
fn invalid_identity_and_unusable_sessions_leave_navigation_unchanged() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    for (owner, action) in [
        (WorkspaceId::new(), Action::Workspace),
        (workspace, Action::Session(SessionId::new())),
        (
            workspace,
            Action::Command {
                session: None,
                name: "invalid",
            },
        ),
        (
            workspace,
            Action::Command {
                session: Some(session),
                name: "diff",
            },
        ),
    ] {
        assert!(runtime.activate_palette_action(owner, &action).is_err());
        assert_eq!(runtime.state().route(), Route::Home(HomeMode::Switch));
    }
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
        [(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Deleting,
                failure_stage: None,
                failure_summary: None,
            },
        )]
        .into(),
    )));
    assert!(!runtime.palette_action_available(workspace, &Action::Session(session)));
}

#[test]
fn session_and_workspace_navigation_close_drawers_without_losing_drafts() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::ToggleDirectorDrawer));
    runtime
        .activate_palette_action(workspace, &Action::Session(session))
        .unwrap();
    assert!(!runtime.state().director_drawer_open());
    assert_eq!(runtime.state().active(), Some(session));
    let _ = runtime.apply_event(AppEvent::Key(AppKey::ToggleRootTerminalDrawer));
    runtime
        .activate_palette_action(workspace, &Action::Workspace)
        .unwrap();
    assert!(!runtime.state().root_terminal_drawer_open());
    assert_eq!(runtime.state().route(), Route::Home(HomeMode::Switch));
    runtime
        .activate_palette_action(
            workspace,
            &Action::Command {
                session: None,
                name: "session",
            },
        )
        .unwrap();
    assert!(runtime.palette_navigation_blocked());
    assert!(
        runtime
            .activate_palette_action(workspace, &Action::Session(session))
            .is_err()
    );
    assert_eq!(runtime.overview_modal().unwrap().input(), "session");
    let _ = runtime.handle_key(Key::Escape);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenOverview));
    assert!(!runtime.palette_navigation_blocked());
    runtime
        .activate_palette_action(workspace, &Action::Session(session))
        .unwrap();
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenNotes));
    assert!(runtime.palette_navigation_blocked());
}

#[test]
fn commands_prepare_the_existing_editors_and_execute_only_on_next_enter() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    for name in ["terminal", "close", "agent"] {
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        runtime
            .activate_palette_action(
                workspace,
                &Action::Command {
                    session: Some(session),
                    name,
                },
            )
            .unwrap();
        assert_eq!(runtime.closeup_modal().unwrap().input(), name);
        assert!(runtime.palette_navigation_blocked());
        assert!(runtime.panes().active_pane().tabs().is_empty());
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects.iter().any(|effect| match (name, effect) {
                (
                    "terminal",
                    Effect::OpenTerminal {
                        target: Target::Session(id),
                        ..
                    },
                )
                | ("close", Effect::RemoveSession { session: id, .. })
                | (
                    "agent",
                    Effect::LaunchAgent {
                        session: Some(id), ..
                    },
                ) => *id == session,
                _ => false,
            }),
            "{effects:?}"
        );
    }
}

#[test]
fn exact_tabs_survive_reordering_but_not_replacement_and_focus_root_scope() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    for (target, kind) in [
        (Target::Session(session), PaneKind::Terminal),
        (Target::Root(workspace), PaneKind::Terminal),
        (Target::Root(workspace), PaneKind::Agent),
    ] {
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let operation = OperationId::new();
        runtime.request_pane(target, operation, kind);
        let pending = runtime.palette_tabs()[0].action.clone();
        runtime
            .activate_palette_action(workspace, &pending)
            .unwrap();
        assert_eq!(runtime.panes().pane(target).unwrap().tabs().len(), 1);
        let live = terminal(workspace, target.session_id());
        runtime.complete_pane(target, operation, live.clone());
        assert!(!runtime.palette_action_available(workspace, &pending));
        let exact = runtime.palette_tabs()[0].action.clone();
        runtime.request_pane(target, OperationId::new(), kind);
        runtime.activate_palette_action(workspace, &exact).unwrap();
        assert_eq!(runtime.focused_terminal(), Some(live.clone()));
        runtime.exit_pane(target, live);
        assert!(!runtime.palette_action_available(workspace, &exact));
    }
}

#[test]
fn interrupted_tab_navigation_never_resumes_and_diff_is_not_searchable() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let operation = OperationId::new();
    runtime.request_pane(target, operation, PaneKind::Diff);
    runtime.inject_pane_event_for_test(target, PaneEvent::Resolved { operation });
    assert!(runtime.palette_tabs().is_empty());
    let history = InterruptedTab {
        continuation: AgentContinuationRef::new(),
        session_id: Some(session),
        last_terminal: terminal(workspace, Some(session)),
        provider: None,
        last_known_phase: None,
        reason: ProviderResumeReason::ProviderMetadataUnavailable,
        target: None,
    };
    let (interaction, revision) = runtime.restore_fence();
    let _ = runtime.restore_snapshot(
        interaction,
        revision,
        vec![PaneRestoreTarget {
            target,
            panes: vec![],
            selected: None,
            selected_interrupted: Some(history.continuation),
            interrupted: vec![history.clone()],
        }],
    );
    let action = runtime.palette_tabs()[0].action.clone();
    runtime.activate_palette_action(workspace, &action).unwrap();
    assert!(runtime.focused_terminal().is_none());
    assert_eq!(
        runtime.focused_interrupted().unwrap().continuation,
        history.continuation
    );
    assert!(matches!(
        action,
        Action::Tab {
            selection: TabSelection::Interrupted(_),
            ..
        }
    ));
}

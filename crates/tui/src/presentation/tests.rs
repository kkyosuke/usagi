#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
use super::{
    AgentCommandPort, AgentCommandPortFactory, AgentPaneAdmission, AgentTabIntentPort,
    AgentTabIntentPortCommit, BTreeMap, BannerScreenRunner, BrowserOpener, Config, ConfigStep,
    ControllerHost, ControllerHostAction, DecisionCommandPort, DefaultSettingsPort,
    DesktopNotificationPort, EnvironmentStorePort, Exit, ExternalTerminalPort, FixedBackendFactory,
    FsSessionWorktreeScanPort, GardenInputRoute, GardenInventoryPort, Geometry, GitDiff, IdleWatch,
    LaunchAgentRequest, MAX_BACKGROUND_EXITS_PER_FRAME, MetricsPort, MetricsPortFactory,
    MissingWorkspacePrompt, NewStep, NoDesktopNotifications, NoMetrics, OpenStep, PROJECT_BAR_ROWS,
    PaneLaunch, PaneLaunchCommandPort, PrModalClickRoute, ProjectedSession,
    SerializedPaneLaunchPort, SessionCommandPort, SessionCommandPortFactory, SessionCommandResult,
    SessionLifecycle, SessionLifecycleProjection, SessionRefreshPort, SessionWorktreeHint,
    SessionWorktreeScanPort, Start, TerminalAttach, TerminalChunk, TerminalError,
    TerminalInputOutcome, TerminalInputResolution, TerminalSubscription, TerminalViewProjection,
    UnavailableAgentCommandPort, UnavailableBackendPort, UnavailableBrowserOpener,
    UnavailableDecisionCommandPort, UnavailableEnvironmentStore, UnavailableExternalTerminalPort,
    UnavailableGardenInventoryPort, UnavailablePaneLaunchPort, UnavailablePrSnapshotPort,
    UnavailableSessionCommandPort, UnavailableSessionCommandPortFactory,
    WORKSPACE_SWITCH_LOADING_GRACE, WelcomeStep, WorkspaceConfigContext, WorkspaceConfigStep,
    WorkspaceCreateCompletion, WorkspaceCreateEffect, WorkspaceCreateToken, WorkspaceDeck,
    WorkspaceInputRoute, WorkspaceIoRuntime, WorkspaceLoader, WorkspaceRuntime, WorkspaceSnapshot,
    WorkspaceView, activate_focused_interrupted_tab, activate_workspace_responsive,
    adjust_project_bar_pointer, app_event_from_key, apply_drawer_header_while_director_open,
    cached_workspace_switch_frame, close_exited_panes, compose_workspace_shell_frame,
    controller_terminal_view, copy_terminal_selection, director_organization,
    dismiss_pr_modal_on_project_bar_click, drain_session_completions,
    focus_workspace_drawer_from_pointer, foreground_terminal_geometry, forward_live_terminal_input,
    garden_click_at, garden_fits, garden_shell_owned_wake, handle_interrupted_removal_confirmation,
    handle_terminal_pointer, home_frame_material, intercept_live_terminal_control,
    is_user_activity, key_to_terminal_bytes, key_to_terminal_bytes_for_mode, new_project_notice,
    open_workspace_responsive, play_startup_splash, poll_and_project_terminals,
    prepare_activation_settings, prepare_batch_settings, prepare_deck_workspace,
    prepare_workspace_deck, projection_build_counts, recent_paths, registry_contains_path,
    remember_workspace_session_focus, remove_registry_paths, render_controller_frame,
    render_home_material, render_home_snapshot, render_missing_workspace_prompt,
    reset_projection_build_counts, restore_open_panes, restore_prepared_workspace,
    restore_workspace_closeup, restore_workspace_session_focus, retarget_drawer_chords,
    route_garden_input, route_pr_modal_click, route_workspace_input_before_reducer,
    run as run_from_start, run_screen_graph_with_backend, run_screen_graph_with_backend_and_notice,
    run_with_settings, run_with_settings_and_agent_and_metrics_port_factory_and_model_availability,
    run_workspace_config, run_workspace_controller, run_workspace_controller_with_backend,
    run_workspace_controller_with_backend_and_config,
    run_workspace_controller_with_backend_and_settings, run_workspace_deck_with_backend_and_config,
    run_workspace_loading, safe_session_error, save_config_responsive,
    save_config_source_responsive, save_environment_responsive, save_setup_commands_responsive,
    select_right_pane_tab, select_root_terminal_tab, sidebar_pointer_event, step_config, step_new,
    step_open, step_workspace_config, terminal_geometry, visit_garden_agent, welcome_action,
    workspace_drawer_header_key, workspace_has_unsaved_surface, workspace_loading_visible,
    write_banner,
};
use crate::presentation::frame::TERMINAL_CURSOR_MARKER;
use crate::presentation::live_terminal::LiveTerminalControls;
use crate::presentation::views::config::AvailableAgentModels;
use crate::presentation::views::new::{Field, Mode, New};
use crate::presentation::views::open::Open;
use crate::presentation::views::welcome::MenuAction;
use crate::presentation::views::workspace::{self, HomeProjection};
use crate::presentation::views::{director_drawer, root_terminal_drawer};
use crate::presentation::widgets::strip_ansi;
use crate::presentation::workspace_runtime::PaneRestoreTarget;
use crate::usecase::application::agent_tab_intent::{
    AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabProjection,
    AgentTabSlotIntent, AgentTabTargetProjection, reconcile_agent_tab_intent_mutation,
};
use crate::usecase::application::controller::WorkspaceDrawerFocus;
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, BackendEvent, DirectorConsoleParent, DirectorNew, DirectorRoute,
    Effect, EnvironmentEntry, GARDEN_IDLE_THRESHOLD, GardenClick, HomeMode, NewRequest, Overlay,
    PendingToken, PreviewFileFilter, RoleEditorScope, Route, SessionCreateIntent, TabDirection,
    Target,
};
use crate::usecase::application::daemon_backend::{
    Completions, DaemonBackend, DecisionPort as BackendDecisionPort, ReopenAgentRequest,
};
use crate::usecase::application::pane::{LivePane, PaneKind, PaneSelection, PaneTab, TabSelection};
use crate::usecase::application::pr::PrSnapshotPort;
use crate::usecase::application::run as dispatch;
use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};
use crate::usecase::application::{EntryScreen, Key, Terminal};
use crate::usecase::overview::SessionCommand;
use crate::usecase::terminal_input::{LiveTerminalAction, PointerEvent, PointerKind};
use chrono::{DateTime, Duration, Timelike, Utc};
use std::collections::{BTreeSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{Receiver, Sender},
};
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::{
    AgentInventory, AgentProfileId, AgentRuntimeInventoryItem, AgentRuntimeInventoryState,
    AgentWorkspaceObservation,
};
use usagi_core::domain::id::{
    AgentContinuationRef, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, OperationId,
    RequestId, SessionId, TerminalId, TerminalRef, UserDecisionId, WorkspaceId, WorktreeId,
};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::session_lifecycle::AgentPhase;
use usagi_core::domain::settings::{AvailableModels, DefaultModel, IconMode, Settings};
use usagi_core::domain::supervisor::{
    ExecutionPolicy, RunProvenance, SupervisorRunId, SupervisorRunQuery, SupervisorRunState, TaskId,
};
use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::usecase::env::EnvScope;
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};

use usagi_core::domain::recent::{Recent, UniteOverview};
use usagi_core::domain::session::{SessionOrigin, SessionRecord};

use crate::usecase::application::daemon_health::DaemonHealthTracker;
use tempfile::tempdir;
use usagi_core::domain::workspace::{Workspace, WorkspaceOverview};
use usagi_core::domain::workspace_state::WorkspaceState;
use usagi_core::infrastructure::client::DaemonMetrics;

/// The unobserved default: diagnostic health draws no indicator, so a frame
/// test keeps asserting the healthy Home frame. The judgement itself is
/// covered by `crate::usecase::application::daemon_health` and by the sidecar tests
/// in [`views::workspace`].
fn health() -> DaemonHealthTracker {
    DaemonHealthTracker::default()
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn observed_work_run(state: SupervisorRunState) -> SupervisorRunQuery {
    SupervisorRunQuery {
        supervisor_run_id: SupervisorRunId::new(),
        state_revision: 1,
        state,
        terminal_at: None,
        terminal_reason: None,
        display_label: Some("Observed Goal".into()),
        root_agent_id: None,
        policy: ExecutionPolicy::default(),
        escalation: None,
        tasks: Vec::new(),
        provenance: Vec::new(),
    }
}

fn with_private_work_run_provenance(mut run: SupervisorRunQuery) -> SupervisorRunQuery {
    run.provenance.push(RunProvenance {
        supervisor_run_id: run.supervisor_run_id,
        task_id: TaskId::new("private-task").unwrap(),
        parent_task_id: None,
        parent_dispatch_run: None,
        dispatch_run_id: OperationId::new(),
        worker_session_id: Some(SessionId::new()),
        worker_agent_id: AgentRuntimeId::new(),
        worker_worktree_id: WorktreeId::new(),
        generation: 1,
    });
    run
}

struct FixedWorkRunControl(super::WorkRunControlResult);

impl super::WorkRunPort for FixedWorkRunControl {
    fn snapshot(
        &mut self,
        _: WorkspaceId,
    ) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
        unreachable!("control test never observes Work Runs")
    }

    fn control(
        &mut self,
        _: WorkspaceId,
        _: OperationId,
        _: usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
    ) -> Result<super::WorkRunControlResult, super::WorkRunControlError> {
        Ok(self.0.clone())
    }
}

fn complete_work_run_control(
    workspace: WorkspaceId,
    request: super::WorkRunControlRequest,
    result: super::WorkRunControlResult,
) -> (
    OperationId,
    Result<super::WorkRunControlResult, super::WorkRunControlError>,
) {
    let (sender, receiver) = std::sync::mpsc::channel();
    super::spawn_work_run_control_job(
        Box::new(FixedWorkRunControl(result)),
        workspace,
        request,
        sender,
    );
    let super::WorkRunLaneCompletion::Control {
        operation_id,
        result,
        ..
    } = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the Work Run control returns its port")
    else {
        panic!("control job returned an observation completion");
    };
    (operation_id, *result)
}

/// Host-action routing without the resident session lane. These tests
/// exercise which action a dispatched effect produces; the lane's own
/// behaviour is covered by [`FakeSessionRefreshPort`] and the frame-loop
/// tests below.
fn drain_host_actions(
    actions: &Receiver<ControllerHostAction>,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    super::drain_controller_host_actions(
        actions,
        ui,
        runtime,
        pending_targets,
        &mut super::UnavailableSessionRefreshPort,
        &mut None,
    );
}

/// A scripted resident session-inventory lane. It records every wake and
/// hands over queued snapshots the way the daemon-backed lane hands over
/// what its worker already fetched — so a test can assert the frame loop
/// only ever drains, and count what the lane was asked to do (#551).
#[derive(Default)]
struct FakeSessionRefreshPort {
    wakes: Arc<AtomicUsize>,
    takes: Arc<AtomicUsize>,
    queued: Arc<Mutex<VecDeque<Result<SessionCommandResult, String>>>>,
}

impl SessionRefreshPort for FakeSessionRefreshPort {
    fn wake(&mut self) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        self.takes.fetch_add(1, Ordering::SeqCst);
        self.queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
    }
}

/// Publish one daemon snapshot on an exact drain observation. The frame
/// loop itself advances `takes`; no wall-clock delay or worker scheduling is
/// involved, so cache invalidation tests can put the change between two
/// already-rendered frames deterministically.
struct ScheduledSessionRefreshPort {
    publish_on_take: usize,
    takes: usize,
    update: Option<SessionCommandResult>,
}

impl SessionRefreshPort for ScheduledSessionRefreshPort {
    fn wake(&mut self) {}

    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        self.takes += 1;
        (self.takes == self.publish_on_take)
            .then(|| self.update.take().map(Ok))
            .flatten()
    }
}

/// A decision lane that counts what the frame loop asked of it. The daemon
/// round trip belongs to the resident worker, so `refresh` here does exactly
/// what the production port does: record a wake and return (#551).
#[derive(Default)]
struct CountingDecisionPort {
    wakes: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}

impl BackendDecisionPort for CountingDecisionPort {
    fn poll(&mut self, _completions: &Completions) {
        self.polls.fetch_add(1, Ordering::SeqCst);
    }

    fn refresh(&mut self, _workspace: WorkspaceId, _completions: Completions) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn resolve(
        &mut self,
        _workspace: WorkspaceId,
        _decision_id: UserDecisionId,
        _answer: UserDecisionAnswer,
        _completions: Completions,
    ) {
    }
}

#[test]
fn app_event_from_key_maps_ordinary_management_keys() {
    assert_eq!(app_event_from_key(Key::Up), Some(AppEvent::Key(AppKey::Up)));
    assert_eq!(
        app_event_from_key(Key::Down),
        Some(AppEvent::Key(AppKey::Down))
    );
    assert_eq!(
        app_event_from_key(Key::PageUp),
        Some(AppEvent::Key(AppKey::PageUp))
    );
    assert_eq!(
        app_event_from_key(Key::PageDown),
        Some(AppEvent::Key(AppKey::PageDown))
    );
    assert_eq!(
        app_event_from_key(Key::Enter),
        Some(AppEvent::Key(AppKey::Enter))
    );
    assert_eq!(
        app_event_from_key(Key::Backspace),
        Some(AppEvent::Key(AppKey::Backspace))
    );
    assert_eq!(
        app_event_from_key(Key::Paste("貼り付け".to_owned())),
        Some(AppEvent::Key(AppKey::Paste("貼り付け".to_owned())))
    );
    assert_eq!(
        app_event_from_key(Key::Tab),
        Some(AppEvent::Key(AppKey::Tab))
    );
    assert_eq!(
        app_event_from_key(Key::Escape),
        Some(AppEvent::Key(AppKey::Escape))
    );
    assert_eq!(
        app_event_from_key(Key::Char('x')),
        Some(AppEvent::Key(AppKey::Char('x')))
    );
    assert_eq!(
        app_event_from_key(Key::Char('\u{1}')),
        Some(AppEvent::Key(AppKey::CtrlA))
    );
    assert_eq!(
        app_event_from_key(Key::Quit),
        Some(AppEvent::Key(AppKey::CtrlC))
    );
    assert_eq!(
        app_event_from_key(Key::CtrlQ),
        Some(AppEvent::Key(AppKey::CtrlQ))
    );
    assert_eq!(
        app_event_from_key(Key::CtrlX),
        Some(AppEvent::Key(AppKey::CtrlX))
    );
    assert_eq!(
        app_event_from_key(Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![0x13],
        }),
        Some(AppEvent::Key(AppKey::SaveRoles))
    );
}

#[test]
fn app_event_from_key_maps_resolved_live_actions_to_reducer_keys() {
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::KeyboardHelp)),
        None
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::Switch)),
        Some(AppEvent::Key(AppKey::CtrlO))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenCloseupModal)),
        Some(AppEvent::Key(AppKey::OpenCloseupOverlay))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::PreviousSession)),
        Some(AppEvent::Key(AppKey::PreviousSession))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::NextSession)),
        Some(AppEvent::Key(AppKey::NextSession))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::NextTab)),
        Some(AppEvent::Key(AppKey::CtrlN))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::PreviousTab)),
        Some(AppEvent::Key(AppKey::CtrlP))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenPullRequests)),
        Some(AppEvent::Key(AppKey::OpenPrs))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenPreview)),
        Some(AppEvent::Key(AppKey::OpenPreview))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenDecisions)),
        Some(AppEvent::Key(AppKey::OpenDecisions))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenNotes)),
        Some(AppEvent::Key(AppKey::OpenNotes))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::OpenGarden)),
        Some(AppEvent::Key(AppKey::OpenGarden))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::Agent)),
        Some(AppEvent::Key(AppKey::CtrlA))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::Director)),
        Some(AppEvent::Key(AppKey::ToggleDirectorDrawer))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::DirectorBack)),
        Some(AppEvent::Key(AppKey::DirectorBack))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::DirectorNew)),
        Some(AppEvent::Key(AppKey::OpenDirectorNew))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::WorkRuns)),
        Some(AppEvent::Key(AppKey::OpenDirectorWorkRuns))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::RootTerminal)),
        Some(AppEvent::Key(AppKey::ToggleRootTerminalDrawer))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::RootTerminalFullHeight)),
        Some(AppEvent::Key(AppKey::ToggleRootTerminalFullHeight))
    );
    assert_eq!(
        app_event_from_key(Key::Live(LiveTerminalAction::QuitConfirmation)),
        Some(AppEvent::Key(AppKey::OpenQuitConfirmation))
    );
}

#[test]
fn closeup_live_pr_action_requests_the_active_sessions_prs_without_an_empty_modal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let mut state =
        crate::usecase::application::controller::AppState::home(workspace, vec![session]);
    let _ = crate::usecase::application::controller::update(
        &mut state,
        AppEvent::PaneTabAvailability {
            available: true,
            error: None,
        },
    );
    let _ =
        crate::usecase::application::controller::update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(state.overlay(), None);

    let event = app_event_from_key(Key::Live(LiveTerminalAction::OpenPullRequests))
        .expect("live PR action maps to a reducer event");
    assert_eq!(
        crate::usecase::application::controller::update(&mut state, event),
        vec![Effect::LoadPullRequests { target }]
    );
    assert_eq!(state.overlay(), None);
    assert_eq!(state.pr_overlay().unwrap().target(), target);
}

#[test]
fn app_event_from_key_ticks_on_wakeups_and_drops_pane_only_input() {
    // Resize / backend wakeups reach the loop as `Other` and advance the mascot.
    assert_eq!(app_event_from_key(Key::Other), Some(AppEvent::Tick));
    // Raw passthrough and terminal pointer drags never reach the Home reducer.
    assert_eq!(app_event_from_key(Key::Passthrough(vec![0x1b])), None);
    // Sidebar clicks need the real runtime's injected monotonic timestamp.
    assert_eq!(app_event_from_key(Key::Click { column: 3, row: 4 }), None);
    // Left/Right reach the reducer to move the Yes/No confirmation focus; the
    // reducer ignores them outside that overlay. Ctrl-D stays Open-only.
    assert_eq!(
        app_event_from_key(Key::Left),
        Some(AppEvent::Key(AppKey::Left))
    );
    assert_eq!(
        app_event_from_key(Key::Right),
        Some(AppEvent::Key(AppKey::Right))
    );
    assert_eq!(app_event_from_key(Key::CtrlD), None);
    assert_eq!(app_event_from_key(Key::Help), None);
    // Tab close and terminal scroll/copy stay pane- and shell-level concerns.
    for action in [
        LiveTerminalAction::CloseTab,
        LiveTerminalAction::ScrollUp,
        LiveTerminalAction::ScrollDown,
        LiveTerminalAction::ScrollBottom,
    ] {
        assert_eq!(app_event_from_key(Key::Live(action)), None);
    }
    let terminal_copy_event = app_event_from_key(Key::TerminalCopy { fallback: vec![3] });
    #[cfg(target_os = "windows")]
    assert_eq!(terminal_copy_event, Some(AppEvent::Key(AppKey::CtrlC)));
    #[cfg(not(target_os = "windows"))]
    assert_eq!(terminal_copy_event, None);
}

#[test]
fn garden_claims_shell_owned_terminal_input_as_wake_events() {
    assert!(garden_shell_owned_wake(&Key::Pointer(PointerEvent {
        kind: crate::usecase::terminal_input::PointerKind::Drag,
        column: 1,
        row: 1,
    })));
    assert!(garden_shell_owned_wake(&Key::Live(
        LiveTerminalAction::ScrollDown
    )));
    assert!(garden_shell_owned_wake(&Key::Passthrough(vec![1])));
    assert!(garden_shell_owned_wake(&Key::Enter));
    assert!(garden_shell_owned_wake(&Key::Quit));
    assert!(garden_shell_owned_wake(&Key::CtrlQ));
    assert!(!garden_shell_owned_wake(&Key::Other));
}

/// A resize is a redraw, never an inventory refresh. It reaches the reducer
/// as the same mascot tick as a wake-up, while the real dimensions come from
/// `term.size()` at the head of the frame; the daemon lanes are not involved
/// at all (#551).
#[test]
fn a_resize_maps_to_a_redraw_tick_and_stays_distinct_from_a_wakeup() {
    assert_eq!(app_event_from_key(Key::Resize), Some(AppEvent::Tick));
    assert_eq!(app_event_from_key(Key::Other), Some(AppEvent::Tick));
    assert_ne!(Key::Resize, Key::Other);
}

#[test]
fn sidebar_pointer_adapter_preserves_coordinates_and_injected_time() {
    let at = std::time::Duration::from_millis(1_234);
    assert_eq!(
        sidebar_pointer_event(3, 4, at),
        AppEvent::Pointer {
            column: 3,
            row: 4,
            at,
        }
    );
}

#[test]
fn pr_modal_click_route_claims_the_box_and_closes_on_its_background() {
    assert_eq!(
        route_pr_modal_click(Some(Overlay::Prs), 24, 80, 0, 2),
        Some(PrModalClickRoute::Inside)
    );
    assert_eq!(
        route_pr_modal_click(Some(Overlay::Prs), 24, 80, 0, 1),
        Some(PrModalClickRoute::Outside)
    );
    assert_eq!(route_pr_modal_click(None, 24, 80, 0, 1), None);
    assert_eq!(
        route_pr_modal_click(Some(Overlay::Notes), 24, 80, 0, 1),
        None
    );
}

#[test]
fn project_bar_click_closes_the_pr_modal_without_reaching_the_bar() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::PullRequestsLoaded {
        target: Target::Session(session),
        revision: 1,
        prs: vec![usagi_core::domain::pr_inventory::PrEntry::new(
            usagi_core::domain::pr_inventory::canonicalize(
                "https://github.com/kkyosuke/usagi/pull/1625",
            )
            .unwrap(),
        )],
    }));
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));

    assert!(dismiss_pr_modal_on_project_bar_click(
        &mut runtime,
        &Key::Click { column: 2, row: 0 }
    ));
    assert_eq!(runtime.state().overlay(), None);

    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenPrs));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));
    assert!(!dismiss_pr_modal_on_project_bar_click(
        &mut runtime,
        &Key::Click { column: 2, row: 1 }
    ));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));
}

fn user_interactions() -> Vec<Key> {
    vec![
        Key::Up,
        Key::Down,
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
        Key::Enter,
        Key::Backspace,
        Key::Tab,
        Key::Escape,
        Key::Quit,
        Key::CtrlQ,
        Key::CtrlD,
        Key::CtrlX,
        Key::Char('g'),
        Key::Paste("pasted".to_owned()),
        Key::Click { column: 4, row: 9 },
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 4,
            row: 9,
        }),
        Key::TerminalCopy { fallback: vec![3] },
        Key::Passthrough(vec![b'x']),
        Key::Live(LiveTerminalAction::ScrollUp),
        Key::Management {
            action: AppKey::Escape,
            passthrough: vec![0x1b],
        },
        // A resize is the user dragging a window edge, and the design has it
        // both close the garden and restart the timer.
        Key::Resize,
    ]
}

/// The screen saver's deadline must survive an Agent working all night and
/// end the moment a person touches the terminal, so the classification of
/// "was that a user?" is pinned over the whole key vocabulary.
#[test]
fn only_a_real_interaction_postpones_the_screen_saver() {
    for key in user_interactions() {
        assert!(is_user_activity(&key), "{key:?} should postpone the garden");
    }
    // The one wake-up that is not a person: frame ticks, drained daemon
    // events, and Agent output all arrive as `Other`.
    assert!(!is_user_activity(&Key::Other));
}

/// The watch is a pure elapsed-time fold: the shell reduces its monotonic
/// clock to a `Duration`, so nothing below it reads a clock at all.
#[test]
fn the_idle_watch_measures_from_the_last_interaction() {
    let ms = std::time::Duration::from_millis;
    let mut watch = IdleWatch::new(ms(1_000));

    assert_eq!(watch.observe(&Key::Other, ms(1_500)), ms(500));
    assert_eq!(watch.observe(&Key::Other, ms(9_000)), ms(8_000));
    // An interaction restarts the measurement in the same call.
    assert_eq!(watch.observe(&Key::Char('a'), ms(9_000)), ms(0));
    assert_eq!(watch.observe(&Key::Other, ms(9_400)), ms(400));
    // A clock that appears to run backwards (it cannot, but the arithmetic
    // must not panic if it ever did) reads as "no idle time".
    assert_eq!(watch.observe(&Key::Other, ms(0)), ms(0));
}

/// The whole automatic path, minus the single thing that is genuinely the
/// OS's — the monotonic clock the shell injects. Frame wake-ups accumulate
/// idle time, an interaction throws it away, the threshold opens the
/// overlay, the very next frame *is* the garden, and a click resolved
/// against that frame's own plots lands in the rabbit's Closeup.
#[test]
#[allow(clippy::too_many_lines)] // End-to-end screen-saver timing and click ownership stay in one scenario.
fn an_idle_home_grows_a_garden_whose_usagi_is_one_click_from_its_closeup() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let record = SessionRecord {
        name: "alpha".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/alpha"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let sessions = vec![ProjectedSession::from_record(session, &record)];
    let no_diffs = BTreeMap::new();
    let clock = now();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let material = |runtime: &WorkspaceRuntime| {
        home_frame_material(
            24,
            80,
            runtime,
            "demo",
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            clock,
        )
    };

    // The composition root's frame period. Idle time is measured in these,
    // and driving them one by one is what proves the threshold is reached
    // rather than assumed.
    let frame = std::time::Duration::from_millis(16);
    let mut watch = IdleWatch::new(std::time::Duration::ZERO);
    let mut elapsed = std::time::Duration::ZERO;
    let wake = |runtime: &mut WorkspaceRuntime,
                watch: &mut IdleWatch,
                elapsed: &mut std::time::Duration,
                key: &Key| {
        *elapsed += frame;
        let idle = watch.observe(key, *elapsed);
        let _ = runtime.apply_event(AppEvent::IdleElapsed(idle));
    };

    // Four minutes of ticks — an Agent streaming output the whole time —
    // and Home is still Home.
    let four_minutes = GARDEN_IDLE_THRESHOLD
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("the threshold is longer than a minute");
    while elapsed < four_minutes {
        wake(&mut runtime, &mut watch, &mut elapsed, &Key::Other);
    }
    assert_eq!(runtime.state().overlay(), None);

    // One keypress and the four minutes are gone.
    wake(&mut runtime, &mut watch, &mut elapsed, &Key::Down);
    let restarted_at = elapsed;
    while elapsed < restarted_at + GARDEN_IDLE_THRESHOLD {
        assert_eq!(
            runtime.state().overlay(),
            None,
            "the garden opened {:?} after the last interaction",
            elapsed.saturating_sub(restarted_at)
        );
        wake(&mut runtime, &mut watch, &mut elapsed, &Key::Other);
    }
    assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));

    // The next frame is the garden itself, drawn full width over Home.
    let garden = material(&runtime);
    let rows = render_home_material(&garden);
    let text = rows.join("\n");
    assert_eq!(rows.len(), 24);
    assert!(
        text.contains("Garden Action Center · click a usagi") && text.contains("any key · wake")
    );
    assert!(text.contains("alpha"));

    // Every cell of the frame resolves through the same layout that drew it,
    // so the rabbit is reachable by clicking where it is drawn and the rest
    // of the garden is a wake-up.
    let resolve = |column: u16, row: u16| {
        garden_click_at(
            garden.height,
            garden.width,
            &garden.projection,
            garden.now,
            column,
            row,
        )
    };
    // Agent の居ない session なので、押せるのは区画（agent 無しの訪問）である。
    let visit = Some(GardenClick::Visit {
        workspace,
        session,
        agent: None,
    });
    let plot = (0..24)
        .flat_map(|row| (0..80).map(move |column| (column, row)))
        .find(|&(column, row)| resolve(column, row) == visit)
        .expect("the garden draws a clickable usagi for its session");
    assert_eq!(resolve(0, 23), Some(GardenClick::Dismiss));

    let _ = runtime.apply_event(AppEvent::GardenClick(
        resolve(plot.0, plot.1).expect("the click landed on the garden"),
    ));
    assert_eq!(runtime.state().active(), Some(session));
    assert!(matches!(
        runtime.state().route(),
        Route::Home(HomeMode::Closeup)
    ));
    // Home is back: the frame after the visit is no longer the garden.
    let after = render_home_material(&material(&runtime)).join("\n");
    assert!(!after.contains("any key to return"));
}

#[test]
fn garden_routes_click_and_pointer_down_through_the_drawn_frame_hit_test() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let record = SessionRecord {
        name: "alpha".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/alpha"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let sessions = vec![ProjectedSession::from_record(session, &record)];
    let no_diffs = BTreeMap::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut pointer_gesture = false;

    for key in [
        Key::Click { column: 0, row: 23 },
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 0,
            row: 23,
        }),
    ] {
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        let material = home_frame_material(
            24,
            80,
            &runtime,
            "demo",
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            now(),
        );
        assert_eq!(
            route_garden_input(
                &mut ui,
                &mut runtime,
                Some(&material),
                &key,
                &mut pointer_gesture,
            ),
            Some(GardenInputRoute::Local(Vec::new())),
        );
        assert_eq!(runtime.state().overlay(), None);
        if pointer_gesture {
            assert_eq!(
                route_garden_input(
                    &mut ui,
                    &mut runtime,
                    None,
                    &Key::Pointer(PointerEvent {
                        kind: PointerKind::Up,
                        column: 0,
                        row: 23,
                    }),
                    &mut pointer_gesture,
                ),
                Some(GardenInputRoute::Local(Vec::new())),
            );
        }
    }
    assert!(!pointer_gesture);
}

#[test]
fn garden_frame_material_uses_every_open_projects_projection() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta.clone()]).unwrap();
    let session = alpha.session_ids[0];
    let sessions = vec![ProjectedSession::from_record(
        session,
        &alpha.state.sessions[0],
    )];
    let mut runtime = WorkspaceRuntime::new(alpha.workspace_id, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));

    // One material build per frame, exactly as the loop does it: the deck
    // projection replaces the active-only Garden rather than folding twice.
    let build = |deck: &WorkspaceDeck| {
        home_frame_material(
            24,
            220,
            &runtime,
            "alpha",
            &sessions,
            None,
            health(),
            &BTreeMap::new(),
            None,
            None,
            now(),
        )
        .with_workspace_deck_garden(deck)
    };
    let material = build(&deck);
    let plots = material
        .projection
        .garden_sessions()
        .expect("the idle Home frame is the Garden");

    assert_eq!(plots.len(), 2);
    assert_eq!(plots[0].label, "alpha / alpha-session");
    assert_eq!(plots[1].label, "beta / beta-session");
    // Until the observation lane reaches it, the other project is drawn
    // read-only: no rabbit is invented for a membership nobody observed.
    assert!(!plots[1].agents_observed);
    let read_only = render_home_material(&material);
    assert!(read_only.iter().any(|row| row.contains("project inactive")));

    // The daemon answers for whichever workspace the request names, so the
    // other project's Agents reach its plot without a resident controller.
    let runtime_id = AgentRuntimeId::new();
    assert!(deck.apply_garden_inventory(&AgentWorkspaceObservation {
        inventory: AgentInventory {
            workspace_id: beta.workspace_id,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef {
                    agent_runtime_id: runtime_id,
                    terminal: TerminalRef {
                        daemon_generation: DaemonGeneration::new(),
                        terminal_id: TerminalId::new(),
                        workspace_id: beta.workspace_id,
                        session_id: Some(beta.session_ids[0]),
                        worktree_id: WorktreeId::new(),
                    },
                    session_id: Some(beta.session_ids[0]),
                },
                continuation: AgentContinuationRef::new(),
                state: AgentRuntimeInventoryState::Live,
                resumed_from: None,
            }],
            resumable: Vec::new(),
        },
        session_statuses: BTreeMap::from([(
            beta.session_ids[0],
            usagi_core::domain::agent::AgentStatus::Idle,
        )]),
    }));
    let material = build(&deck);
    let plots = material
        .projection
        .garden_sessions()
        .expect("the idle Home frame is the Garden");
    assert_eq!(
        plots[1]
            .agents
            .iter()
            .map(|agent| (agent.runtime_id, agent.phase))
            .collect::<Vec<_>>(),
        vec![(runtime_id, AgentPhase::Running)]
    );
    let observed = render_home_material(&material);
    assert!(!observed.iter().any(|row| row.contains("project inactive")));
    assert!(observed.iter().any(|row| row.contains("2 sessions")));
    assert!(observed.iter().any(|row| row.contains("1 usagi")));
    assert!(observed.iter().any(|row| row.contains("-.-")));
    assert!(observed.iter().any(|row| row.contains("completed")));
    assert!(!observed.iter().any(|row| row.contains("1 run")));
}

#[test]
fn garden_arrow_wakes_home_without_reaching_the_surface_behind_it() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let mut material = home_frame_material(
        23,
        80,
        &runtime,
        "demo",
        &[],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    material.projection = material.projection.with_deck_garden(
            "5 open projects".to_owned(),
            (0..5)
                .map(|index| {
                    (
                        WorkspaceId::new(),
                        crate::presentation::widgets::garden::GardenSession {
                            sidebar: crate::presentation::widgets::garden::sidebar::SessionDetails::default(),
                            id: SessionId::new(),
                            label: format!("project-{index} / session-{index}"),
                            lifecycle: SessionLifecycle::Available,
                            failure_summary: None,
                            agents_observed: false,
                            agents: Vec::new(),
                            agent_status: None,
                            pending_decisions: 0,
                            pr_merged: false,
                        },
                    )
                })
                .collect(),
        );
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut pointer_gesture = false;

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            Some(&material),
            &Key::Right,
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert_eq!(runtime.state().overlay(), None);
}

#[test]
fn documented_garden_minimum_includes_the_project_bar_row() {
    assert!(garden_fits(14 - PROJECT_BAR_ROWS, 64));
    assert!(!garden_fits(13 - PROJECT_BAR_ROWS, 64));
}

#[test]
fn garden_list_keys_and_wheel_scroll_without_waking_the_terminal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut gesture = false;
    let mut material = home_frame_material(
        24,
        120,
        &runtime,
        "demo",
        &[],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    material.projection = material.projection.with_deck_garden(
        "demo".into(),
        vec![(
            workspace,
            crate::presentation::widgets::garden::GardenSession {
                sidebar: crate::presentation::widgets::garden::sidebar::SessionDetails::default(),
                id: session,
                label: "many agents".into(),
                lifecycle: SessionLifecycle::Available,
                failure_summary: None,
                agents_observed: true,
                agents: (0..30)
                    .map(|_| crate::presentation::widgets::garden::GardenAgent {
                        runtime_id: AgentRuntimeId::new(),
                        phase: AgentPhase::Running,
                    })
                    .collect(),
                agent_status: None,
                pending_decisions: 0,
                pr_merged: false,
            },
        )],
    );
    for (key, expected) in [
        (Key::Down, 1),
        (Key::Up, 0),
        (Key::PageDown, 13),
        (Key::PageUp, 0),
        (
            Key::Live(LiveTerminalAction::Wheel {
                up: false,
                column: 119,
                row: 5,
                notches: 3,
            }),
            3,
        ),
        (
            Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: 119,
                row: 5,
                notches: 3,
            }),
            0,
        ),
    ] {
        assert_eq!(
            route_garden_input(&mut ui, &mut runtime, Some(&material), &key, &mut gesture),
            Some(GardenInputRoute::Local(Vec::new()))
        );
        assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));
        assert_eq!(runtime.state().garden_sidebar_scroll(), expected);
    }
    let wheel_outside = Key::Live(LiveTerminalAction::Wheel {
        up: false,
        column: 0,
        row: 5,
        notches: 1,
    });
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            Some(&material),
            &wheel_outside,
            &mut gesture
        ),
        Some(GardenInputRoute::Local(Vec::new()))
    );
    assert_eq!(runtime.state().overlay(), None);
}

#[test]
fn garden_list_scroll_survives_the_shell_gate_and_redraws_before_the_next_input() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Management {
            action: AppKey::OpenGarden,
            passthrough: Vec::new(),
        },
        Key::Down,
        Key::Down,
        Key::Up,
        Key::PageDown,
        Key::PageUp,
        Key::Live(LiveTerminalAction::Wheel {
            up: false,
            column: 119,
            row: 5,
            notches: 3,
        }),
        Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 119,
            row: 5,
            notches: 3,
        }),
        Key::Click {
            column: 119,
            row: 24,
        },
        Key::Click {
            column: 85,
            row: 24,
        },
        Key::Quit,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    term.size = Some((25, 120));
    let mut factory = FixedBackendFactory {
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
    let snapshot = snapshot_with_sessions(
        "scroll-list",
        &[
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        ],
    );
    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
        Exit::Quit
    );
    let gardens = term
        .frames
        .iter()
        .filter(|frame| frame.join("\n").contains("Garden Action Center"))
        .collect::<Vec<_>>();
    assert_eq!(gardens.len(), 10, "every list input redraws the Garden");
    let top = |frame: &Vec<String>| {
        super::widgets::strip_ansi(&frame[3])
            .rsplit('│')
            .next()
            .unwrap()
            .trim()
            .to_owned()
    };
    assert!(top(gardens[0]).contains("scroll-list"));
    assert!(top(gardens[1]).contains("alpha"));
    assert!(top(gardens[2]).contains("usagi/alpha"));
    assert_eq!(top(gardens[3]), top(gardens[1]));
    assert_ne!(top(gardens[4]), top(gardens[3]));
    assert_eq!(top(gardens[5]), top(gardens[0]));
    assert!(top(gardens[6]).contains("No agent activity."));
    assert_eq!(top(gardens[7]), top(gardens[0]));
    assert_eq!(top(gardens[8]), top(gardens[4]));
    assert_eq!(top(gardens[9]), top(gardens[0]));
}

#[test]
fn narrow_garden_list_input_wakes_home_through_the_frame_loop() {
    for key in [
        Key::Down,
        Key::Live(LiveTerminalAction::Wheel {
            up: false,
            column: 79,
            row: 5,
            notches: 3,
        }),
    ] {
        let mut term = FakeTerminal::with_keys(&[
            Key::Management {
                action: AppKey::OpenGarden,
                passthrough: Vec::new(),
            },
            key,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        term.size = Some((25, 80));
        let mut factory = FixedBackendFactory {
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
            run_workspace_controller_with_backend(&mut term, snapshot("narrow"), &mut factory)
                .unwrap(),
            Exit::Quit
        );
        assert_eq!(
            term.frames
                .iter()
                .filter(|frame| { frame.join("\n").contains("Garden Action Center") })
                .count(),
            1,
            "the list input wakes a Garden without a sidebar"
        );
    }
}

#[test]
fn garden_routes_an_inactive_projects_agent_row_to_the_deck_shell() {
    let workspace = WorkspaceId::new();
    let foreign_workspace = WorkspaceId::new();
    let session = SessionId::new();
    let agent = AgentRuntimeId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let mut material = home_frame_material(
        24,
        120,
        &runtime,
        "demo",
        &[],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    material.projection = material.projection.with_deck_garden(
        "2 open projects".to_owned(),
        vec![(
            foreign_workspace,
            crate::presentation::widgets::garden::GardenSession {
                sidebar: crate::presentation::widgets::garden::sidebar::SessionDetails::default(),
                id: session,
                label: "other / review".to_owned(),
                lifecycle: SessionLifecycle::Available,
                failure_summary: None,
                agents_observed: true,
                agents: vec![crate::presentation::widgets::garden::GardenAgent {
                    runtime_id: agent,
                    phase: AgentPhase::Waiting,
                }],
                agent_status: Some(usagi_core::domain::agent::AgentStatus::Running),
                pending_decisions: 0,
                pr_merged: false,
            },
        )],
    );
    let click = (0..24)
        .flat_map(|row| (85..120).map(move |column| (column, row)))
        .find(|&(column, row)| {
            matches!(
                garden_click_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    column,
                    row,
                ),
                Some(GardenClick::Visit {
                    workspace,
                    agent: Some(runtime),
                    ..
                }) if workspace == foreign_workspace && runtime == agent
            )
        })
        .expect("the inactive project's Agent row is clickable");
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut pointer_gesture = false;

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            Some(&material),
            &Key::Click {
                column: click.0,
                row: click.1,
            },
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Project(super::GardenProjectVisit {
            workspace: foreign_workspace,
            session,
            agent: Some(agent),
        })),
    );
    assert_eq!(runtime.state().overlay(), None);
}

/// Any other press is the documented wake-up: it is consumed, and the Home
/// from before the screen saver comes back with no target changed.
#[test]
fn a_click_beside_the_usagi_only_wakes_the_garden() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));

    let before = runtime.state().active();
    let _ = runtime.apply_event(AppEvent::GardenClick(GardenClick::Dismiss));
    assert_eq!(runtime.state().overlay(), None);
    assert_eq!(runtime.state().active(), before);
}

#[test]
fn garden_consumes_every_kind_of_user_input_before_the_terminal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pointer_gesture = false;
    for key in user_interactions() {
        let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));
        assert_eq!(
            route_garden_input(&mut ui, &mut runtime, None, &key, &mut pointer_gesture,),
            Some(GardenInputRoute::Local(Vec::new())),
            "Garden did not consume {key:?}",
        );
        assert_eq!(runtime.state().overlay(), None);
        if pointer_gesture {
            assert_eq!(
                route_garden_input(
                    &mut ui,
                    &mut runtime,
                    None,
                    &Key::Pointer(PointerEvent {
                        kind: PointerKind::Up,
                        column: 0,
                        row: 0,
                    }),
                    &mut pointer_gesture,
                ),
                Some(GardenInputRoute::Local(Vec::new())),
            );
        }
        assert!(!pointer_gesture);
    }

    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &Key::Other,
            &mut pointer_gesture,
        ),
        None,
    );
    assert_eq!(runtime.state().overlay(), Some(Overlay::Garden));
}

#[test]
fn garden_pointer_press_owns_its_drag_and_release_after_dismissal() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let mut pointer_gesture = false;
    let pointer = |kind| {
        Key::Pointer(PointerEvent {
            kind,
            column: 4,
            row: 9,
        })
    };

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Drag),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Up),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(!pointer_gesture);
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Down),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert_eq!(runtime.state().overlay(), None);
    assert!(pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Drag),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Up),
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Local(Vec::new())),
    );
    assert!(!pointer_gesture);
    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            None,
            &pointer(PointerKind::Up),
            &mut pointer_gesture,
        ),
        None,
    );
}

/// うさぎの click は session を訪問したうえで、その agent 自身の tab を開く。
/// 区画（nameplate や余白）の click は従来どおり session の Closeup までで、
/// tab 選択を動かさない。
#[test]
fn clicking_a_usagi_opens_that_agents_tab() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
    };
    let second = TerminalRef {
        terminal_id: TerminalId::new(),
        ..first.clone()
    };
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        interaction,
        revision,
        vec![PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second.clone(),
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let runtime_id = AgentRuntimeId::new();
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::RuntimePhase {
        runtime: AgentRuntimeRef {
            agent_runtime_id: runtime_id,
            terminal: second.clone(),
            session_id: Some(session),
        },
        phase: usagi_core::domain::session_lifecycle::AgentPhase::Running,
    }));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));

    // 区画の click（agent 無し）は tab を動かさない。
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let plot_click = GardenClick::Visit {
        workspace,
        session,
        agent: None,
    };
    let _ = runtime.apply_event(AppEvent::GardenClick(plot_click));
    visit_garden_agent(&mut ui, &mut runtime, plot_click);
    assert_eq!(runtime.focused_terminal(), Some(first.clone()));

    // うさぎの click は、その runtime を持つ tab を選ぶ。
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let rabbit_click = GardenClick::Visit {
        workspace,
        session,
        agent: Some(runtime_id),
    };
    let _ = runtime.apply_event(AppEvent::GardenClick(rabbit_click));
    visit_garden_agent(&mut ui, &mut runtime, rabbit_click);
    assert_eq!(runtime.focused_terminal(), Some(second.clone()));

    // 押した瞬間に終了していたうさぎは、無関係な tab を選ばない（選択はそのまま）。
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let gone = GardenClick::Visit {
        workspace,
        session,
        agent: Some(AgentRuntimeId::new()),
    };
    let _ = runtime.apply_event(AppEvent::GardenClick(gone));
    visit_garden_agent(&mut ui, &mut runtime, gone);
    assert_eq!(runtime.focused_terminal(), Some(second));

    // Dismiss は訪問ですらないので、agent の焦点も動かさない。
    visit_garden_agent(&mut ui, &mut runtime, GardenClick::Dismiss);
}

/// tab strip がまだ無い session では、うさぎの click は空の Closeup への
/// 訪問までで止まり、pane を勝手に選ばない。
#[test]
fn a_rabbit_click_on_a_tabless_session_stops_at_its_closeup() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));

    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let click = GardenClick::Visit {
        workspace,
        session,
        agent: Some(AgentRuntimeId::new()),
    };
    // The Garden overlay owns input before the visit reaches Closeup, so a
    // covered pane strip cannot receive the agent click.
    visit_garden_agent(&mut ui, &mut runtime, click);
    assert_eq!(runtime.focused_terminal(), None);
    let _ = runtime.apply_event(AppEvent::GardenClick(click));
    assert_eq!(runtime.state().overlay(), None);
    visit_garden_agent(&mut ui, &mut runtime, click);
    assert_eq!(runtime.focused_terminal(), None);
}

#[test]
fn failed_delete_selection_renders_the_force_remove_confirmation() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let record = SessionRecord {
        name: "feature".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/feature"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let mut projected = ProjectedSession::from_record(session, &record);
    projected.lifecycle = SessionLifecycle::Failed;
    projected.failure_stage = Some(usagi_core::domain::session_lifecycle::FailureStage::Delete);
    projected.failure_summary = Some("safe detail".to_owned());
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
        BTreeMap::from([(
            session,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Delete),
                failure_summary: Some("safe detail".to_owned()),
            },
        )]),
    )));
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));

    let material = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &[projected],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    );
    let frame = render_home_material(&material).join("\n");

    assert!(frame.contains("Force remove"));
    assert!(frame.contains("Force remove feature?"));
    assert!(frame.contains("[ yes ]"));
    assert!(frame.contains("[ no  ]"));
    assert!(frame.contains("Previous removal failed. Changes may be discarded."));
}

fn ws(name: &str) -> Workspace {
    Workspace::new(name, format!("/tmp/{name}"))
}

fn ws_minutes_ago(name: &str, minutes: i64) -> Workspace {
    let mut workspace = ws(name);
    workspace.updated_at = now() - Duration::minutes(minutes);
    workspace
}

fn state(name: &str) -> WorkspaceState {
    WorkspaceState {
        sessions: vec![SessionRecord {
            name: format!("{name}-session"),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: PathBuf::from(format!("/tmp/{name}/session")),
            created_at: now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        }],
        root_notes: Scratchpad::default(),
        updated_at: now(),
    }
}

fn empty_state(name: &str) -> WorkspaceState {
    let mut state = state(name);
    state.sessions.clear();
    state
}

fn snapshot_with_generated_runtime_ids(
    workspace: Workspace,
    state: WorkspaceState,
) -> WorkspaceSnapshot {
    let session_ids = state.sessions.iter().map(|_| SessionId::new()).collect();
    WorkspaceSnapshot::with_runtime_ids(workspace, state, WorkspaceId::new(), session_ids)
}

fn snapshot(name: &str) -> WorkspaceSnapshot {
    snapshot_with_generated_runtime_ids(ws(name), state(name))
}

fn snapshot_with_sessions(name: &str, session_names: &[&str]) -> WorkspaceSnapshot {
    let mut workspace_state = state(name);
    let template = workspace_state.sessions[0].clone();
    workspace_state.sessions = session_names
        .iter()
        .map(|session| SessionRecord {
            name: (*session).to_owned(),
            root: PathBuf::from(format!("/tmp/{name}/{session}")),
            ..template.clone()
        })
        .collect();
    snapshot_with_generated_runtime_ids(ws(name), workspace_state)
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
#[allow(clippy::too_many_lines)] // One host fixture verifies the complete routing matrix.
fn backend_host_and_explicit_error_adapters_cover_the_full_route_matrix() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    )
    .with_decisions(Box::new(UnavailableBackendPort))
    .with_overlay(Box::new(UnavailableBackendPort));

    for effect in [
        Effect::CreateSession {
            workspace,
            token: PendingToken::from_raw(1),
            operation_id: OperationId::new(),
            intent: SessionCreateIntent {
                name: "feature".to_owned(),
                base_ref: None,
                profile: None,
                model: None,
                role_id: None,
            },
        },
        Effect::RefreshSessions { workspace },
        Effect::RemoveSession {
            workspace,
            session,
            force: true,
            force_delete_branch: false,
            purge_orphan: false,
        },
        Effect::SleepSession { workspace, session },
        Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: OperationId::new(),
            profile: None,
        },
        Effect::ResumeAgent {
            workspace,
            session,
            operation_id: OperationId::new(),
        },
        Effect::ReopenAgent {
            workspace,
            continuation: AgentContinuationRef::new(),
        },
        Effect::OpenTerminal {
            target,
            operation_id: OperationId::new(),
            arguments: "new".to_owned(),
        },
        Effect::OpenExternalTerminal { target },
        Effect::SelectTab {
            direction: TabDirection::Next,
        },
    ] {
        backend.dispatch(effect);
    }
    assert_eq!(actions.try_iter().count(), 10);

    for effect in [
        Effect::LoadNotes { target },
        Effect::SaveNotes {
            target,
            scratchpad: Scratchpad::default(),
        },
        Effect::LoadEnvironment {
            scope: EnvScope::Workspace,
        },
        Effect::SaveEnvironment {
            scope: EnvScope::Workspace,
            entries: vec![EnvironmentEntry {
                name: "KEY".to_owned(),
                value: "value".to_owned(),
            }],
        },
        Effect::WorkspaceCommand {
            workspace,
            command: crate::usecase::overview::Command::Issue {
                arguments: "list".to_owned(),
            },
        },
        Effect::RefreshDecisions { workspace },
        Effect::ResolveDecision {
            workspace,
            decision_id: UserDecisionId::new(),
            answer: UserDecisionAnswer::Freeform {
                text: "answer".to_owned(),
            },
        },
        Effect::LoadPullRequests { target },
        Effect::LoadPreview {
            target,
            request_id: RequestId::new(),
            path: None,
            filter: PreviewFileFilter::All,
        },
        Effect::OpenPullRequest {
            url: "https://github.com/o/r/pull/1".to_owned(),
        },
    ] {
        backend.dispatch(effect);
    }
    assert_eq!(backend.drain_events().len(), 10);
}

#[test]
fn default_agent_port_rejects_legacy_inventory_and_exact_resume() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut port = SuccessfulAgentPort(live_terminal_ref(workspace, session));
    assert_eq!(
        port.resume(workspace, session, OperationId::new())
            .unwrap_err(),
        "Agent resume is unavailable."
    );
    assert_eq!(
        port.resume_inventory(workspace).unwrap_err(),
        "Agent resume inventory is unavailable."
    );
    let target = usagi_core::domain::agent::AgentResumeTarget {
        continuation: usagi_core::domain::id::AgentContinuationRef::new(),
        source: usagi_core::domain::id::AgentResumeSourceId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
        runtime_id: usagi_core::domain::id::AgentRuntimeId::new(),
        adapter_revision: 1,
    };
    assert_eq!(
        port.resume_exact(target, OperationId::new()).unwrap_err(),
        "Exact Agent resume is unavailable."
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One host fixture verifies the ordered action-routing contract.
fn controller_host_executor_routes_busy_launch_terminal_and_tab_actions() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    );
    let token = PendingToken::from_raw(90);

    for effect in [
        Effect::CreateSession {
            workspace,
            token,
            operation_id: OperationId::new(),
            intent: SessionCreateIntent {
                name: "feature".into(),
                base_ref: None,
                profile: None,
                model: None,
                role_id: None,
            },
        },
        Effect::RefreshSessions { workspace },
        Effect::RemoveSession {
            workspace,
            session: SessionId::new(),
            force: false,
            force_delete_branch: false,
            purge_orphan: false,
        },
        Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: OperationId::new(),
            profile: None,
        },
        Effect::ResumeAgent {
            workspace,
            session,
            operation_id: OperationId::new(),
        },
        Effect::ReopenAgent {
            workspace: WorkspaceId::new(),
            continuation: AgentContinuationRef::new(),
        },
        Effect::OpenTerminal {
            target,
            operation_id: OperationId::new(),
            arguments: "new".into(),
        },
        Effect::OpenExternalTerminal { target },
        Effect::OpenExternalTerminal {
            target: Target::Session(SessionId::new()),
        },
        Effect::SelectTab {
            direction: TabDirection::Previous,
        },
    ] {
        backend.dispatch(effect);
    }
    drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
    let completed = (0..1)
        .map(|_| {
            ui.session_completions
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("session command completion")
        })
        .collect::<Vec<_>>();
    for completion in completed {
        ui.session_completion_sender.send(completion).unwrap();
    }
    super::drain_session_completions(&mut ui);
    let events = backend.drain_events();
    // Create is admitted and reports its token; Remove is refused as busy
    // and notices. `RefreshSessions` no longer competes for that single
    // command slot at all — it parks on the resident lane, which observes
    // nothing here, so it contributes no event (#551).
    assert_eq!(events.len(), 2);
    assert!(events.iter().any(|event| matches!(
        event,
        AppEvent::OperationResult(result) if result.token == token && !result.succeeded
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AppEvent::Backend(BackendEvent::Notice(_))))
            .count(),
        1
    );
    assert_eq!(ui.pane_launches.len(), 2);
    assert!(!pending.is_empty());

    let calls = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(SnapshotSessionPort(calls.clone())))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(SuccessfulAgentPort(live_terminal_ref(workspace, session))),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    );
    backend.dispatch(Effect::RefreshSessions { workspace });
    drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
    std::thread::sleep(std::time::Duration::from_millis(10));
    super::drain_session_completions(&mut ui);
    backend.dispatch(Effect::SleepSession { workspace, session });
    drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
    std::thread::sleep(std::time::Duration::from_millis(10));
    super::drain_session_completions(&mut ui);
    backend.dispatch(Effect::RemoveSession {
        workspace,
        session,
        force: true,
        force_delete_branch: false,
        purge_orphan: false,
    });
    drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
    std::thread::sleep(std::time::Duration::from_millis(10));
    super::drain_session_completions(&mut ui);
    backend.dispatch(Effect::SleepSession {
        workspace,
        session: SessionId::new(),
    });
    drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
    backend.dispatch(Effect::OpenTerminal {
        target,
        operation_id: OperationId::new(),
        arguments: "new".into(),
    });
    drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
    // Only the user-initiated `Sleep` and `Remove` reach the command port. The
    // refresh went to the resident lane, so it neither spawned a worker nor
    // opened a connection of its own (#551).
    assert_eq!(
        calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        2
    );
}

#[test]
fn right_pane_click_selection_reaches_the_runtime_tab_owner() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    let first_operation = OperationId::new();
    let first = live_terminal_ref(workspace, session);
    let _ = runtime.request_pane(target, first_operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(target, first_operation, first.clone());
    let second_operation = OperationId::new();
    let second = live_terminal_ref(workspace, session);
    let _ = runtime.request_pane(target, second_operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(target, second_operation, second);
    // A stale/out-of-range frame hit is inert.
    select_right_pane_tab(&mut ui, &mut runtime, usize::MAX);
    select_right_pane_tab(&mut ui, &mut runtime, 0);

    assert_eq!(runtime.focused_terminal(), Some(first));
}

#[test]
fn right_pane_agent_click_commits_intent_before_selection_and_surfaces_failure() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let first = live_terminal_ref(workspace, session);
    let second = live_terminal_ref(workspace, session);
    let continuation = AgentContinuationRef::new();
    let interrupted = interrupted_history(workspace, Some(session), true);
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: first.clone(),
        select: true,
    });
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: interrupted.continuation,
        terminal: interrupted.last_terminal.clone(),
        select: false,
    });
    let durable = Arc::new(Mutex::new(intent.clone()));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    for terminal in [first.clone(), second.clone()] {
        let operation = OperationId::new();
        let _ = runtime.request_pane(target, operation, PaneKind::Agent);
        let _ = runtime.complete_pane(target, operation, terminal);
    }
    runtime.inject_pane_event_for_test(
        target,
        crate::usecase::application::pane::PaneEvent::RestoreInterrupted {
            tabs: vec![interrupted.clone()],
        },
    );
    let pending = OperationId::new();
    let _ = runtime.request_pane(target, pending, PaneKind::Agent);

    let interrupted_index = runtime
            .active_pane()
            .tabs()
            .iter()
            .position(|tab| {
                matches!(tab, PaneTab::Interrupted(pane) if pane.tab.continuation == interrupted.continuation)
            })
            .expect("interrupted tab is visible");
    let pending_index = runtime
        .active_pane()
        .tabs()
        .iter()
        .position(|tab| matches!(tab, PaneTab::Pending(pane) if pane.operation == pending))
        .expect("pending tab is visible");
    select_right_pane_tab(&mut ui, &mut runtime, pending_index);
    select_right_pane_tab(&mut ui, &mut runtime, interrupted_index);

    select_right_pane_tab(&mut ui, &mut runtime, 1);

    assert_eq!(runtime.focused_terminal(), Some(second.clone()));
    assert!(mutations.lock().unwrap().iter().any(|mutation| matches!(
        mutation,
        AgentTabIntentMutation::Select {
            session_id: Some(selected),
            continuation: None,
        } if *selected == session
    )));

    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut failing_ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::new(Mutex::new(intent)),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    select_right_pane_tab(&mut failing_ui, &mut runtime, 0);

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.focused_terminal(), Some(second));
}

#[test]
fn a_failed_lifecycle_flows_to_the_sidebar_rows_and_the_reducer() {
    use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    view.set_session_lifecycles(std::collections::BTreeMap::from([(
        session,
        SessionLifecycleProjection {
            lifecycle: SessionLifecycle::Failed,
            failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Create),
            failure_summary: Some("create failed".into()),
        },
    )]));
    let role = crate::usecase::application::controller::SessionRoleProjection {
        role_id: None,
        role_summary: Some("Reviewer".into()),
        parent_session_id: None,
        agent_status: None,
    };
    view.set_session_roles(BTreeMap::from([(session, role.clone())]));
    let ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));

    // The projected sidebar row carries the Failed lifecycle and its reason.
    let mut state =
        crate::usecase::application::controller::AppState::home(workspace, vec![session]);
    let _ = crate::usecase::application::controller::update(
        &mut state,
        crate::usecase::application::controller::AppEvent::Backend(
            crate::usecase::application::controller::BackendEvent::PullRequestsLoaded {
                target: Target::Session(session),
                revision: 1,
                prs: vec![usagi_core::domain::pr_inventory::PrEntry::new(
                    usagi_core::domain::pr_inventory::canonicalize(
                        "https://github.com/example/repository/pull/1545",
                    )
                    .unwrap(),
                )],
            },
        ),
    );
    let rows = super::project_controller_sessions(&ui, &state);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].lifecycle, SessionLifecycle::Failed);
    assert_eq!(rows[0].failure_summary.as_deref(), Some("create failed"));
    assert!(!rows[0].removing);
    assert!(rows[0].pr_count > 0);

    // The reducer receives the lifecycle so it can gate attach by capability.
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    super::sync_runtime_sessions(&mut runtime, &ui, &[]);
    assert_eq!(runtime.state().sessions(), &[session]);
    assert_eq!(runtime.state().session_roles().get(&session), Some(&role));
    assert_eq!(
        runtime
            .state()
            .session_lifecycles()
            .get(&session)
            .map(|projection| projection.lifecycle),
        Some(SessionLifecycle::Failed),
    );
}

#[test]
fn sidebar_groups_children_and_navigation_survives_snapshot_refresh() {
    use crate::usecase::application::controller::{
        AppEvent, AppKey, Selection, SessionRoleProjection, Target,
    };

    let ids = (0..6).map(|_| SessionId::new()).collect::<Vec<_>>();
    let mut snapshot = state("demo");
    let template = snapshot.sessions[0].clone();
    snapshot.sessions = [
        "session",
        "agy",
        "review",
        "inject-user-environment",
        "display",
        "grandchild",
    ]
    .into_iter()
    .map(|name| SessionRecord {
        name: name.into(),
        ..template.clone()
    })
    .collect();
    let records = snapshot.sessions.clone();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), snapshot, ids.clone());
    let roles = BTreeMap::from([(ids[3], ids[0]), (ids[5], ids[3])].map(|(id, parent)| {
        (
            id,
            SessionRoleProjection {
                role_id: None,
                role_summary: None,
                parent_session_id: Some(parent),
                agent_status: None,
            },
        )
    }));
    view.set_session_roles(roles.clone());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(WorkspaceId::new(), vec![ids[0]]);
    super::sync_runtime_sessions(&mut runtime, &ui, &[]);
    let expected = [ids[0], ids[3], ids[5], ids[1], ids[2], ids[4]];
    assert_eq!(runtime.state().sessions(), &expected);
    let rows = super::project_controller_sessions(&ui, runtime.state());
    assert_eq!(
        rows.iter()
            .map(|row| (row.label.as_str(), row.organization_depth))
            .collect::<Vec<_>>(),
        vec![
            ("session", 0),
            ("inject-user-environment", 1),
            ("grandchild", 2),
            ("agy", 0),
            ("review", 0),
            ("display", 0),
        ]
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Down));
    assert_eq!(
        runtime.state().selected(),
        Selection::Target(Target::Session(ids[3]))
    );
    let active = runtime.state().active();
    ui.workspace.replace_sessions_with_runtime_ids(records, ids);
    ui.workspace.set_session_roles(roles);
    super::sync_runtime_sessions(&mut runtime, &ui, &[]);
    assert_eq!(runtime.state().sessions(), &expected);
    assert_eq!(
        runtime.state().selected(),
        Selection::Target(Target::Session(expected[1]))
    );
    assert_eq!(runtime.state().active(), active);
    let revision = ui.workspace.material_revision();
    ui.workspace
        .set_session_roles(ui.workspace.session_roles().clone());
    assert_eq!(ui.workspace.material_revision(), revision);
}

#[test]
fn director_organization_projects_statuses_hierarchy_and_orphans() {
    use usagi_core::domain::agent::AgentStatus;

    let mut empty_state = state("empty");
    empty_state.sessions.clear();
    let empty_ui = WorkspaceIoRuntime::new(
        WorkspaceView::with_runtime_ids(ws("empty"), empty_state, Vec::new()),
        Box::new(UnavailableSessionCommandPort),
    );
    assert_eq!(director_organization(&empty_ui)[0].label, "♛ Director");

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
    let role = |parent_session_id, agent_status| {
        crate::usecase::application::controller::SessionRoleProjection {
            role_id: None,
            role_summary: None,
            parent_session_id,
            agent_status,
        }
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
    let ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));

    let rows = director_organization(&ui);
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
    super::sync_runtime_sessions(&mut runtime, &ui, &[]);
    let projected = super::project_controller_sessions(&ui, runtime.state());
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

#[test]
fn a_deleting_lifecycle_keeps_the_row_marked_removing_without_a_local_command() {
    use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
    let session = SessionId::new();
    let mut view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    view.set_session_lifecycles(std::collections::BTreeMap::from([(
        session,
        SessionLifecycleProjection {
            lifecycle: SessionLifecycle::Deleting,
            failure_stage: None,
            failure_summary: None,
        },
    )]));
    let ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));

    // The daemon accepts a removal before its worktree teardown runs, so the
    // row stays marked as being removed on the strength of the daemon's
    // lifecycle alone — this TUI never issued the command.
    let state =
        crate::usecase::application::controller::AppState::home(WorkspaceId::new(), vec![session]);
    let rows = super::project_controller_sessions(&ui, &state);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].lifecycle, SessionLifecycle::Deleting);
    assert!(rows[0].removing);
}

#[test]
#[allow(clippy::too_many_lines)] // One shell fixture keeps port absence and async completion in sequence.
fn workspace_shell_harness_covers_port_absence_projection_and_async_launch_completion() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let terminal = live_terminal_ref(workspace, session);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));

    ui.start_terminal_session(terminal.clone(), Geometry { cols: 20, rows: 5 });
    ui.set_allowed_agent_sessions(BTreeSet::new());
    let allowed_sessions = BTreeSet::from([session]);
    ui.set_allowed_agent_sessions(allowed_sessions.iter().copied());
    ui.resize_terminals(Geometry { cols: 20, rows: 5 });
    assert!(ui.send_terminal_bytes(&terminal, b"x").is_err());
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(
        super::session_name_for(&ui, session).as_deref(),
        Some("demo-session")
    );
    assert_eq!(super::session_name_for(&ui, SessionId::new()), None);

    let records = ui.workspace.sessions().to_vec();
    super::apply_session_projection(&mut ui, None, None, None, None, None);
    super::apply_session_projection(&mut ui, Some(records.clone()), None, None, None, None);
    assert!(ui.workspace.sessions().is_empty());
    assert!(ui.workspace.session_ids().is_empty());
    super::apply_session_projection(
        &mut ui,
        Some(records),
        Some(vec![session]),
        None,
        None,
        None,
    );
    let records = ui.workspace.sessions().to_vec();
    super::apply_session_projection(
        &mut ui,
        Some(records),
        Some(vec![session]),
        Some(std::collections::BTreeMap::new()),
        Some(std::collections::BTreeMap::new()),
        None,
    );
    let mut mismatched_runtime = WorkspaceRuntime::new(workspace, Vec::new());
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Terminal {
                operation: OperationId::new(),
                result: Err("late completion without an Agent port".to_owned()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut mismatched_runtime,
        &mut std::collections::HashMap::new(),
        Geometry { cols: 20, rows: 5 },
    );
    super::sync_runtime_sessions(&mut mismatched_runtime, &ui, &[]);
    let mut no_controls = LiveTerminalControls::default();
    let _ = super::poll_and_project_terminals(
        &mut ui,
        &mut mismatched_runtime,
        &mut no_controls,
        Geometry { cols: 20, rows: 5 },
    );
    let mut ui = ui
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(SuccessfulAgentPort(terminal.clone())),
        )
        .with_pane_launch_port(launch_port(Box::new(SuccessfulAgentPort(terminal.clone()))));
    assert!(ui.send_terminal_bytes(&terminal, b"missing").is_err());
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    for session in [Some(session), None] {
        ui.pane_launches.push(super::PaneLaunch::Agent {
            operation: OperationId::new(),
            workspace,
            session,
            profile: None,
            goal: None,
            resume: true,
        });
        super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
        drain_completions_at(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
            1,
            Geometry { cols: 20, rows: 5 },
        );
    }
    let operation = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: operation,
        profile: None,
    });
    ui.pane_launches.push(super::PaneLaunch::Agent {
        operation,
        workspace,
        session: Some(session),
        profile: None,
        goal: None,
        resume: false,
    });
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
    drain_completions_at(
        &mut ui,
        &mut runtime,
        &mut pending,
        1,
        Geometry { cols: 20, rows: 5 },
    );
    assert!(pending.is_empty());
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "a successful Agent launch must replace the pre-launch inventory"
    );
    ui.resize_terminals(Geometry { cols: 30, rows: 6 });
    let projected_records = ui.workspace.sessions().to_vec();
    super::apply_session_projection(
        &mut ui,
        Some(projected_records),
        Some(vec![session]),
        None,
        Some(std::collections::BTreeMap::from([(
            session,
            usagi_core::domain::session_lifecycle::SessionLifecycleProjection {
                lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
                failure_stage: None,
                failure_summary: None,
            },
        )])),
        None,
    );

    let operation = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: operation,
        arguments: "new".into(),
    });
    ui.pane_launches.push(super::PaneLaunch::Terminal {
        operation,
        workspace,
        session: Some(session),
        arguments: "new".into(),
    });
    pending.insert(operation, target);
    super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
    drain_completions_at(
        &mut ui,
        &mut runtime,
        &mut pending,
        1,
        Geometry { cols: 20, rows: 5 },
    );
    assert_eq!(
        runtime
            .state()
            .terminal_launch_error()
            .map(|notice| notice.message.as_str()),
        Some("terminal launch is unavailable")
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));

    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Agent {
                operation: OperationId::new(),
                result: Ok(AgentPaneAdmission {
                    terminal: terminal.clone(),
                    continuation: None,
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "a daemon admission still changes inventory after its pending tab closes"
    );

    let failed_agent = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: failed_agent,
        profile: None,
    });
    pending.insert(failed_agent, target);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Agent {
                operation: failed_agent,
                result: Err("safe Agent launch failure".to_owned()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert!(pending.is_empty());
    assert!(
        !ui.take_agent_inventory_change_observation_request(),
        "a rejected Agent launch does not change daemon inventory"
    );
    assert_eq!(runtime.state().overlay(), Some(Overlay::AgentLaunchError));
    assert_eq!(
        runtime
            .state()
            .agent_launch_error()
            .map(|notice| notice.message.as_str()),
        Some("safe Agent launch failure")
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));

    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Terminal {
                operation: OperationId::new(),
                result: Err("late terminal failure".to_owned()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );

    let failed_terminal = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: failed_terminal,
        arguments: "open".into(),
    });
    pending.insert(failed_terminal, target);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Terminal {
                operation: failed_terminal,
                result: Err("login shell could not be started".to_owned()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert_eq!(
        runtime.state().overlay(),
        Some(Overlay::TerminalLaunchError)
    );
    assert_eq!(
        runtime
            .state()
            .terminal_launch_error()
            .map(|notice| notice.message.as_str()),
        Some("login shell could not be started")
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));

    let cancel = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: cancel,
        arguments: "open".into(),
    });
    ui.pane_launches.push(super::PaneLaunch::Terminal {
        operation: cancel,
        workspace,
        session: Some(session),
        arguments: "open".into(),
    });
    pending.insert(cancel, target);
    let _ = runtime.select_tab(TabDirection::Next);
    super::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending);

    // Two more requests: admission takes exactly one worker and leaves the
    // rest visibly pending, without ever touching the stream port.
    assert!(ui.poll_all_terminals().is_empty());
    let queued_before = ui.pane_launches.len();
    ui.pane_launches.push(super::PaneLaunch::Agent {
        operation: OperationId::new(),
        workspace,
        session: Some(session),
        profile: None,
        goal: None,
        resume: false,
    });
    ui.pane_launches.push(super::PaneLaunch::Terminal {
        operation: OperationId::new(),
        workspace,
        session: Some(session),
        arguments: "open".into(),
    });
    super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
    assert_eq!(ui.pane_launches.len(), queued_before + 1);
    assert!(ui.active_pane_launch.is_some());
    assert!(ui.poll_all_terminals().is_empty());
}

/// Every resident-stream call the live panes made, so a test can prove pane IO
/// continued while a launch worker was stopped inside the daemon client.
#[derive(Default)]
struct StreamCalls {
    /// Launches asked of the *stream* port. It must stay 0: launches belong to
    /// the dedicated launch client.
    launches: usize,
    attaches: usize,
    /// The viewport each attach stated: a window claims its share of the
    /// terminal's geometry with the attach itself.
    attach_geometries: Vec<(TerminalRef, Geometry)>,
    polls: usize,
    poll_terminals: Vec<TerminalRef>,
    scripted_polls: Vec<(TerminalRef, Vec<TerminalChunk>)>,
    background_watches: Vec<Vec<TerminalRef>>,
    inputs: Vec<Vec<u8>>,
    resizes: usize,
    resize_geometries: Vec<(TerminalRef, Geometry)>,
    /// The shared viewport this daemon answers a resize with, when it is not
    /// the request (another window holds this terminal smaller).
    effective_geometry: Option<Geometry>,
    detaches: usize,
}

struct RecordingStreamPort(Arc<Mutex<StreamCalls>>);

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=a_blocked_pane_launch_keeps_every_live_pane_streaming
impl AgentCommandPort for RecordingStreamPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        self.0.lock().unwrap().launches += 1;
        Err("the resident stream port never launches".to_owned())
    }

    fn attach_terminal(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        let mut calls = self.0.lock().unwrap();
        calls.attaches += 1;
        calls.attach_geometries.push((terminal.clone(), geometry));
        drop(calls);
        Ok(TerminalAttach {
            subscription: TerminalSubscription { id: 9, epoch: 1 },
            revision: 1,
            output_offset: b"one\r\ntwo\r\nthree".len() as u64,
            next_input_seq: None,
            screen: attach_checkpoint(b"one\r\ntwo\r\nthree", geometry),
            exited: false,
        })
    }

    fn poll_terminal(
        &mut self,
        terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        let mut calls = self.0.lock().unwrap();
        calls.polls += 1;
        calls.poll_terminals.push(terminal.clone());
        let Some(position) = calls
            .scripted_polls
            .iter()
            .position(|(candidate, _)| candidate.fences(terminal))
        else {
            return Ok(Vec::new());
        };
        Ok(calls.scripted_polls.remove(position).1)
    }

    fn input_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _subscription: TerminalSubscription,
        _input_seq: u64,
        _operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        self.0.lock().unwrap().inputs.push(bytes.to_vec());
        Ok(TerminalInputOutcome::Written)
    }

    fn resize_terminal(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        let mut calls = self.0.lock().unwrap();
        calls.resizes += 1;
        calls.resize_geometries.push((terminal.clone(), geometry));
        Ok(calls.effective_geometry.unwrap_or(geometry))
    }

    fn detach_terminal(&mut self, _terminal: &TerminalRef, _subscription: TerminalSubscription) {
        self.0.lock().unwrap().detaches += 1;
    }

    fn watch_background_terminals(&mut self, terminals: &[TerminalRef]) {
        self.0
            .lock()
            .unwrap()
            .background_watches
            .push(terminals.to_vec());
    }
}

/// A launch client the test stops inside the request: `entered` announces the
/// admitted request, `release` lets it answer, and `finished` reports that the
/// worker left the client. Nothing here sleeps, so the barrier is exact.
struct GatedLaunchPort {
    terminal: TerminalRef,
    entered: Mutex<std::sync::mpsc::Sender<&'static str>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    finished: Mutex<std::sync::mpsc::Sender<&'static str>>,
}

impl GatedLaunchPort {
    fn gate(&self, kind: &'static str) {
        let _ = self.entered.lock().unwrap().send(kind);
        let _ = self.release.lock().unwrap().recv();
        let _ = self.finished.lock().unwrap().send(kind);
    }
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=a_blocked_pane_launch_keeps_every_live_pane_streaming
impl PaneLaunchCommandPort for GatedLaunchPort {
    fn launch(
        &self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        self.gate("launch");
        Ok(AgentPaneAdmission {
            terminal: self.terminal.clone(),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn resume(
        &self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        self.gate("resume");
        Err("resume is not scripted".to_owned())
    }

    fn resume_exact(
        &self,
        _target: AgentResumeTarget,
        _operation: OperationId,
    ) -> Result<ExactAgentResume, String> {
        self.gate("resume_exact");
        Err("exact resume is not scripted".to_owned())
    }

    fn launch_terminal(
        &self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        _operation: OperationId,
    ) -> Result<TerminalRef, String> {
        self.gate("launch_terminal");
        Ok(self.terminal.clone())
    }
}

/// A launch client that dies on its first request and answers the next one, so
/// a test can prove the shared client survives a worker's unwind.
struct PanickingLaunchPort {
    terminal: TerminalRef,
    calls: Arc<AtomicUsize>,
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=a_panicking_launch_worker_fails_only_its_pane
impl PaneLaunchCommandPort for PanickingLaunchPort {
    fn launch(
        &self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(call > 0, "launch client died");
        Ok(AgentPaneAdmission {
            terminal: self.terminal.clone(),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn resume(
        &self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("resume is not scripted".to_owned())
    }

    fn resume_exact(
        &self,
        _target: AgentResumeTarget,
        _operation: OperationId,
    ) -> Result<ExactAgentResume, String> {
        Err("exact resume is not scripted".to_owned())
    }

    fn launch_terminal(
        &self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        _operation: OperationId,
    ) -> Result<TerminalRef, String> {
        Err("terminal launch is not scripted".to_owned())
    }
}

/// A `WorkspaceIoRuntime` whose resident stream records its calls and whose launches
/// go to a separate, test-controlled client.
fn ui_with_split_ports(
    workspace: WorkspaceId,
    session: SessionId,
    stream: Arc<Mutex<StreamCalls>>,
    launch: Box<dyn PaneLaunchCommandPort>,
) -> WorkspaceIoRuntime {
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RecordingStreamPort(stream)),
        )
        .with_pane_launch_port(launch)
}

fn agent_launch(
    workspace: WorkspaceId,
    session: SessionId,
    operation: OperationId,
) -> super::PaneLaunch {
    super::PaneLaunch::Agent {
        operation,
        workspace,
        session: Some(session),
        profile: None,
        goal: None,
        resume: false,
    }
}

/// Take exactly `count` completions off the worker channel, then put them back
/// through the drain the frame loop uses. Nothing sleeps, so the number of
/// completions each request produced is exact.
fn drain_completions_at(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending: &mut std::collections::HashMap<OperationId, Target>,
    count: usize,
    geometry: Geometry,
) -> Vec<super::PaneLaunchOutcome> {
    let taken = (0..count)
        .map(|_| {
            ui.pane_completions
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("every admitted request owes exactly one completion")
        })
        .collect::<Vec<_>>();
    let outcomes = taken
        .iter()
        .map(|completion| completion.outcome.clone())
        .collect();
    for completion in taken {
        ui.pane_completion_sender
            .send(completion)
            .expect("the workspace still owns its completion receiver");
    }
    super::drain_pane_completions_into_runtime(ui, runtime, pending, geometry);
    outcomes
}

fn drain_completions(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending: &mut std::collections::HashMap<OperationId, Target>,
    count: usize,
) -> Vec<super::PaneLaunchOutcome> {
    drain_completions_at(ui, runtime, pending, count, terminal_geometry(20, 80))
}

fn drain_next_completion(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending: &mut std::collections::HashMap<OperationId, Target>,
) -> super::PaneLaunchOutcome {
    drain_completions(ui, runtime, pending, 1).remove(0)
}

#[test]
#[allow(clippy::too_many_lines)] // One barrier fixture keeps both panes' IO in sequence.
fn a_blocked_pane_launch_keeps_every_live_pane_streaming() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let first = live_terminal_ref(workspace, session);
    let second = live_terminal_ref(workspace, session);
    let launched = scoped_terminal_ref(workspace, Some(session));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished) = std::sync::mpsc::channel();
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(GatedLaunchPort {
            terminal: launched.clone(),
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            finished: Mutex::new(finished_tx),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    ui.start_terminal_session(first.clone(), terminal_geometry(20, 80));
    ui.start_terminal_session(second.clone(), terminal_geometry(20, 80));
    assert_eq!(stream.lock().unwrap().attaches, 2);

    let blocked = OperationId::new();
    let queued = OperationId::new();
    let mut pending = std::collections::HashMap::new();
    for operation in [blocked, queued] {
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        pending.insert(operation, target);
        super::enqueue_pane_launch(&mut ui, agent_launch(workspace, session, operation));
    }

    // One worker is admitted and stops inside the launch client. A second
    // drain must not start another request on it.
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(ui.pane_launches.len(), 1);
    assert!(ui.active_pane_launch.is_some());
    assert!(entered.try_recv().is_err());

    // The stopped worker owns nothing the live panes need: both keep polling,
    // accepting input, resizing, and detaching.
    let resizes_before = stream.lock().unwrap().resizes;
    ui.resize_terminals(terminal_geometry(24, 100));
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(ui.send_terminal_bytes(&first, b"a"), Ok(()));
    assert_eq!(ui.send_terminal_bytes(&second, b"b"), Ok(()));
    ui.close_terminal(&second);
    {
        let observed = stream.lock().unwrap();
        assert_eq!(observed.launches, 0);
        assert_eq!(observed.resizes, resizes_before + 2);
        assert_eq!(observed.polls, 2);
        assert_eq!(observed.inputs, [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(observed.detaches, 1);
    }

    // Releasing the barrier completes exactly the admitted pane and frees
    // admission for the one that stayed pending.
    release.send(()).unwrap();
    assert_eq!(
        finished.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    assert!(matches!(
        outcome,
        super::PaneLaunchOutcome::Agent { operation, result: Ok(admission) }
            if operation == blocked && admission.terminal.fences(&launched)
    ));
    assert!(ui.active_pane_launch.is_none());
    assert!(!pending.contains_key(&blocked));
    assert!(pending.contains_key(&queued));

    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    assert!(ui.pane_launches.is_empty());
    release.send(()).unwrap();
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    assert!(matches!(
        outcome,
        super::PaneLaunchOutcome::Agent { operation, .. } if operation == queued
    ));
    assert!(pending.is_empty());
    // Two requests, two completions, and the stream port was never asked to
    // launch anything.
    assert_eq!(stream.lock().unwrap().launches, 0);
    assert!(ui.pane_completions.try_recv().is_err());
}

/// One request the identity-recording launch client answered.
#[derive(Debug, Clone)]
struct RecordedLaunch {
    kind: &'static str,
    operation: OperationId,
    terminal: TerminalRef,
}

/// A launch client that records the operation each request carried and mints a
/// terminal for exactly that operation. A completion applied to the wrong
/// pending pane is therefore visible as a foreign terminal on that pane.
struct IdentityRecordingLaunchPort(Arc<Mutex<Vec<RecordedLaunch>>>);

impl IdentityRecordingLaunchPort {
    fn record(
        &self,
        kind: &'static str,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
    ) -> TerminalRef {
        let terminal = scoped_terminal_ref(workspace, session);
        self.0.lock().unwrap().push(RecordedLaunch {
            kind,
            operation,
            terminal: terminal.clone(),
        });
        terminal
    }
}

impl PaneLaunchCommandPort for IdentityRecordingLaunchPort {
    fn launch(
        &self,
        operation: OperationId,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Ok(AgentPaneAdmission {
            terminal: self.record("agent", operation, workspace, session),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn launch_goal(
        &self,
        operation: OperationId,
        workspace: WorkspaceId,
        _profile: Option<AgentProfileId>,
        _goal: &str,
    ) -> Result<AgentPaneAdmission, String> {
        Ok(AgentPaneAdmission {
            terminal: self.record("goal", operation, workspace, None),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn resume(
        &self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("resume is not part of this fixture".to_owned())
    }

    fn resume_exact(
        &self,
        _target: AgentResumeTarget,
        _operation: OperationId,
    ) -> Result<ExactAgentResume, String> {
        Err("resume is not part of this fixture".to_owned())
    }

    fn launch_terminal(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        operation: OperationId,
    ) -> Result<TerminalRef, String> {
        Ok(self.record("terminal", operation, workspace, session))
    }
}

/// The live terminals `target`'s pane currently shows.
fn live_tab_terminals(runtime: &WorkspaceRuntime, target: Target) -> Vec<TerminalRef> {
    runtime
        .panes()
        .pane(target)
        .map(|pane| {
            pane.tabs()
                .iter()
                .filter_map(|tab| match tab {
                    PaneTab::Live(live) => Some(live.terminal.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// #522: the operation the controller issued for a pending pane is the one the
/// launch client is asked with — Agent and generic terminal, workspace root and
/// session alike. No adapter mints a second identity whose side effect could be
/// promoted into this pane.
#[test]
fn every_pane_launch_request_carries_its_own_pending_operation() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::new(Mutex::new(StreamCalls::default())),
        Box::new(IdentityRecordingLaunchPort(Arc::clone(&requests))),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();

    let root_agent = OperationId::new();
    let root_goal = OperationId::new();
    let session_agent = OperationId::new();
    let session_terminal = OperationId::new();
    let planned = [
        (
            Target::Root(workspace),
            root_agent,
            "agent",
            super::PaneLaunch::Agent {
                operation: root_agent,
                workspace,
                session: None,
                profile: None,
                goal: None,
                resume: false,
            },
        ),
        (
            Target::Root(workspace),
            root_goal,
            "goal",
            super::PaneLaunch::Agent {
                operation: root_goal,
                workspace,
                session: None,
                profile: None,
                goal: Some("prepare the PR".to_owned()),
                resume: false,
            },
        ),
        (
            Target::Session(session),
            session_agent,
            "agent",
            agent_launch(workspace, session, session_agent),
        ),
        (
            Target::Session(session),
            session_terminal,
            "terminal",
            super::PaneLaunch::Terminal {
                operation: session_terminal,
                workspace,
                session: Some(session),
                arguments: "new".into(),
            },
        ),
    ];
    let expected = planned
        .iter()
        .map(|(target, operation, kind, _)| (*target, *operation, *kind))
        .collect::<Vec<_>>();
    for (target, operation, _, launch) in planned {
        runtime.request_pane(target, operation, PaneKind::Agent);
        pending.insert(operation, target);
        super::enqueue_pane_launch(&mut ui, launch);
    }

    // One worker at a time, each drained before the next is admitted.
    for (target, operation, kind) in expected {
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        drain_next_completion(&mut ui, &mut runtime, &mut pending);
        let recorded = requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("each admitted launch reaches the client once");
        assert_eq!(recorded.kind, kind);
        assert_eq!(
            recorded.operation, operation,
            "the pending pane's own operation is what the daemon is asked with"
        );
        assert!(
            live_tab_terminals(&runtime, target).contains(&recorded.terminal),
            "the pane promoted the terminal its own operation was answered with"
        );
    }
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(pending.is_empty());
}

#[test]
fn goal_pane_launch_rejects_a_managed_session_before_calling_the_port() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let operation = OperationId::new();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let outcome = super::run_pane_launch(
        &IdentityRecordingLaunchPort(Arc::clone(&requests)),
        super::PaneLaunch::Agent {
            operation,
            workspace,
            session: Some(session),
            profile: None,
            goal: Some("invalid scope".to_owned()),
            resume: false,
        },
        terminal_geometry(20, 80),
    );

    assert!(matches!(
        outcome,
        super::PaneLaunchOutcome::Agent {
            operation: actual,
            result: Err(ref reason),
        } if actual == operation && reason.contains("workspace-root scope")
    ));
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn goal_host_action_creates_one_root_pending_pane_and_preserves_the_goal() {
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let mut pending = std::collections::HashMap::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::LaunchAgent(LaunchAgentRequest {
            workspace,
            session: None,
            operation_id: operation,
            profile: None,
            goal: Some("prepare a PR".to_owned()),
        }))
        .unwrap();

    drain_host_actions(&receiver, &mut ui, &mut runtime, &mut pending);

    assert_eq!(pending.get(&operation), Some(&Target::Root(workspace)));
    assert!(matches!(
        runtime
            .panes()
            .pane(Target::Root(workspace))
            .and_then(|pane| pane.tabs().last()),
        Some(PaneTab::Pending(pending)) if pending.operation == operation
    ));
    assert!(matches!(
        ui.pane_launches.as_slice(),
        [super::PaneLaunch::Agent {
            operation: actual,
            session: None,
            goal: Some(goal),
            ..
        }] if *actual == operation && goal == "prepare a PR"
    ));
}

/// #522: while a pending operation lives in this process, its completion — even
/// applied out of order — promotes only its own pane, and a completion that
/// arrives after the pending tab is gone revives nothing.
#[test]
fn out_of_order_and_late_completions_never_cross_or_revive_a_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root = Target::Root(workspace);
    let scoped = Target::Session(session);
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::new(Mutex::new(StreamCalls::default())),
        Box::new(IdentityRecordingLaunchPort(Arc::new(
            Mutex::new(Vec::new()),
        ))),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();

    let first = OperationId::new();
    let second = OperationId::new();
    let closed = OperationId::new();
    let first_terminal = scoped_terminal_ref(workspace, None);
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let closed_terminal = scoped_terminal_ref(workspace, Some(session));
    for (target, operation) in [(root, first), (scoped, second), (scoped, closed)] {
        runtime.request_pane(target, operation, PaneKind::Agent);
        pending.insert(operation, target);
    }
    // The third pane is dropped before its daemon answer arrives.
    runtime.fail_pane(scoped, closed, "cancelled".to_owned());

    // The answers arrive in the reverse of the order they were requested.
    for (operation, terminal) in [
        (closed, closed_terminal.clone()),
        (second, second_terminal.clone()),
        (first, first_terminal.clone()),
    ] {
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                launch_id: super::PANE_LAUNCH_UNADMITTED,
                outcome: super::PaneLaunchOutcome::Agent {
                    operation,
                    result: Ok(AgentPaneAdmission {
                        terminal,
                        continuation: None,
                        supervisor_run_id: None,
                    }),
                },
            })
            .expect("the workspace still owns its completion receiver");
    }
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    assert_eq!(live_tab_terminals(&runtime, root), vec![first_terminal]);
    assert_eq!(live_tab_terminals(&runtime, scoped), vec![second_terminal]);
    assert!(
        !live_tab_terminals(&runtime, scoped).contains(&closed_terminal),
        "a completion for a closed pending tab never revives it"
    );
    assert!(pending.is_empty());
}

#[test]
fn a_panicking_launch_worker_fails_only_its_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let live = live_terminal_ref(workspace, session);
    let launched = scoped_terminal_ref(workspace, Some(session));
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(PanickingLaunchPort {
            terminal: launched.clone(),
            calls: Arc::clone(&calls),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    ui.start_terminal_session(live.clone(), terminal_geometry(20, 80));
    let dying = OperationId::new();
    let next = OperationId::new();
    let mut pending = std::collections::HashMap::new();
    for operation in [dying, next] {
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        pending.insert(operation, target);
        super::enqueue_pane_launch(&mut ui, agent_launch(workspace, session, operation));
    }

    // The worker unwinds inside the client. Its pane still gets exactly one
    // safe failure, and the shared client is not lost with the thread.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    std::panic::set_hook(hook);
    assert!(matches!(
        outcome,
        super::PaneLaunchOutcome::Agent { operation, result: Err(message) }
            if operation == dying && message == super::PANE_LAUNCH_WORKER_FAILED
    ));
    assert!(ui.active_pane_launch.is_none());
    assert!(!pending.contains_key(&dying));

    // The live pane never noticed, and the next launch reaches the daemon.
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(ui.send_terminal_bytes(&live, b"c"), Ok(()));
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    let outcome = drain_next_completion(&mut ui, &mut runtime, &mut pending);
    assert!(matches!(
        outcome,
        super::PaneLaunchOutcome::Agent { operation, result: Ok(admission) }
            if operation == next && admission.terminal.fences(&launched)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(pending.is_empty());
    assert_eq!(stream.lock().unwrap().launches, 0);
}

#[test]
fn launch_admission_is_bounded_and_refuses_beyond_the_queue_with_one_busy_completion() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let launched = scoped_terminal_ref(workspace, Some(session));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, _finished) = std::sync::mpsc::channel();
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(GatedLaunchPort {
            terminal: launched,
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            finished: Mutex::new(finished_tx),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let mut admitted = Vec::new();
    for _ in 0..super::PANE_LAUNCH_QUEUE_LIMIT {
        let operation = OperationId::new();
        admitted.push(operation);
        super::enqueue_pane_launch(&mut ui, agent_launch(workspace, session, operation));
    }
    assert_eq!(ui.pane_launches.len(), super::PANE_LAUNCH_QUEUE_LIMIT);

    // The queue is full: the next requests never reach the daemon and each
    // completes exactly once as Busy — Agent, generic terminal, and explicit
    // tab resume alike.
    let history = interrupted_history(workspace, Some(session), true);
    let refused_kinds = [
        agent_launch(workspace, session, OperationId::new()),
        super::PaneLaunch::Terminal {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            arguments: "new".into(),
        },
        super::PaneLaunch::ResumeExact {
            operation: OperationId::new(),
            continuation: history.continuation,
            target: history.target.clone().unwrap(),
        },
    ];
    let refused = refused_kinds
        .iter()
        .map(|launch| launch.identity().operation())
        .collect::<Vec<_>>();
    for (launch, operation) in refused_kinds.into_iter().zip(refused.iter().copied()) {
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session: Some(session),
            operation_id: operation,
            profile: None,
        });
        pending.insert(operation, target);
        super::enqueue_pane_launch(&mut ui, launch);
    }
    assert_eq!(ui.pane_launches.len(), super::PANE_LAUNCH_QUEUE_LIMIT);

    // One worker is admitted first, so the Busy completions must not free it.
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    let active = ui.active_pane_launch;
    assert!(active.is_some());
    let outcomes = drain_completions(&mut ui, &mut runtime, &mut pending, refused.len());
    for (outcome, operation) in outcomes.iter().zip(refused.iter().copied()) {
        let (completed, message) = match outcome {
            super::PaneLaunchOutcome::Agent { operation, result } => {
                (*operation, result.as_ref().err().cloned())
            }
            super::PaneLaunchOutcome::Terminal { operation, result } => {
                (*operation, result.as_ref().err().cloned())
            }
            super::PaneLaunchOutcome::ResumeExact {
                operation, result, ..
            } => (*operation, result.as_ref().err().cloned()),
        };
        assert_eq!(completed, operation);
        assert_eq!(message.as_deref(), Some(super::PANE_LAUNCH_BUSY));
    }
    // A Busy completion is unadmitted: it never frees the running worker.
    assert_eq!(ui.active_pane_launch, active);
    assert!(pending.is_empty());
    assert!(ui.pane_completions.try_recv().is_err());
    release.send(()).unwrap();
}

#[test]
fn a_stale_completion_neither_frees_admission_nor_completes_another_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let stale_terminal = scoped_terminal_ref(workspace, Some(session));
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(UnavailablePaneLaunchPort),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let waiting = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: waiting,
        profile: None,
    });
    let mut pending = std::collections::HashMap::from([(waiting, target)]);
    // A newer worker owns admission.
    ui.active_pane_launch = Some(7);

    // A completion from an older worker, for an operation nobody waits for.
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: 3,
            outcome: super::PaneLaunchOutcome::Agent {
                operation: OperationId::new(),
                result: Ok(AgentPaneAdmission {
                    terminal: stale_terminal,
                    continuation: None,
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    assert_eq!(ui.active_pane_launch, Some(7));
    assert_eq!(pending.get(&waiting), Some(&target));
    assert_eq!(runtime.focused_terminal(), None);
}

#[test]
fn a_workspace_exit_never_strands_the_shared_launch_client() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let launched = scoped_terminal_ref(workspace, Some(session));
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished) = std::sync::mpsc::channel();
    let stream = Arc::new(Mutex::new(StreamCalls::default()));
    let mut ui = ui_with_split_ports(
        workspace,
        session,
        Arc::clone(&stream),
        Box::new(GatedLaunchPort {
            terminal: launched,
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            finished: Mutex::new(finished_tx),
        }),
    );
    super::enqueue_pane_launch(
        &mut ui,
        agent_launch(workspace, session, OperationId::new()),
    );
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(
        entered.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );

    // The workspace exits while the worker is still inside the client: its
    // completion receiver is gone, so the send is dropped harmlessly and the
    // borrowed client outlives the UI instead of being lost with it.
    drop(ui);
    release.send(()).unwrap();
    assert_eq!(
        finished.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("launch")
    );
    assert_eq!(stream.lock().unwrap().launches, 0);
}

#[test]
#[allow(clippy::too_many_lines)] // One sequence fixes both completion kinds and persisted selection.
fn successful_pane_completions_persist_focus_and_select_agent_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let agent_terminal = scoped_terminal_ref(workspace, Some(session));
    let generic_terminal = scoped_terminal_ref(workspace, Some(session));
    let continuation = AgentContinuationRef::new();
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(SuccessfulAgentPort(agent_terminal.clone())),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending = std::collections::HashMap::new();
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    assert_eq!(runtime.panes().active(), Some(Target::Session(session)));

    let agent_operation = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: agent_operation,
        profile: None,
    });
    pending.insert(agent_operation, target);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Agent {
                operation: agent_operation,
                result: Ok(AgentPaneAdmission {
                    terminal: agent_terminal.clone(),
                    continuation: Some(continuation),
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert_eq!(runtime.focused_terminal(), Some(agent_terminal.clone()));
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&agent_terminal)
    );

    let terminal_operation = OperationId::new();
    runtime.on_effect(&Effect::OpenTerminal {
        target,
        operation_id: terminal_operation,
        arguments: "new".into(),
    });
    pending.insert(terminal_operation, target);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Terminal {
                operation: terminal_operation,
                result: Ok(generic_terminal.clone()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );
    assert!(pending.is_empty());
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal));

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Previous))
        .unwrap();
    drain_host_actions(&receiver, &mut ui, &mut runtime, &mut pending);
    assert_eq!(runtime.focused_terminal(), Some(agent_terminal));
    assert!(matches!(
        mutations.lock().unwrap().last(),
        Some(AgentTabIntentMutation::Select {
            session_id: Some(actual),
            continuation: Some(actual_continuation),
        }) if *actual == session && *actual_continuation == continuation
    ));
}

#[test]
fn drawer_new_root_completion_commits_one_selected_exact_tab_across_reopen() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let continuation = AgentContinuationRef::new();
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
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
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(
        runtime
            .handle_key(Key::Live(LiveTerminalAction::DirectorNew))
            .is_empty()
    );
    let effects = runtime.handle_key(Key::Enter);
    let [
        effect @ Effect::LaunchAgent {
            session: None,
            operation_id,
            profile: Some(profile),
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("drawer confirmation must emit one explicit root launch: {effects:?}");
    };
    assert_eq!(profile.as_str(), "claude");
    runtime.on_effect(effect);
    let mut pending = std::collections::HashMap::from([(*operation_id, Target::Root(workspace))]);

    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Agent {
                operation: *operation_id,
                result: Ok(AgentPaneAdmission {
                    terminal: terminal.clone(),
                    continuation: Some(continuation),
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );

    assert!(pending.is_empty());
    assert_eq!(runtime.state().director_launching(), None);
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    let intent = durable.lock().unwrap();
    assert_eq!(intent.targets.len(), 1);
    assert_eq!(intent.targets[0].session_id, None);
    assert_eq!(intent.targets[0].selected, Some(continuation));
    assert_eq!(intent.targets[0].tabs.len(), 1);
    assert!(intent.targets[0].tabs[0].terminal.fences(&terminal));
    drop(intent);

    let tabs = runtime.active_pane().tabs().to_vec();
    let _ = runtime.handle_key(Key::Escape);
    assert!(runtime.state().director_drawer_open());
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    let _ = runtime.handle_key(Key::Escape);
    assert!(!runtime.state().director_drawer_open());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.active_pane().tabs(), tabs.as_slice());
    assert_eq!(
        mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|mutation| matches!(
                mutation,
                AgentTabIntentMutation::Upsert {
                    session_id: None,
                    continuation: actual,
                    select: true,
                    ..
                } if *actual == continuation
            ))
            .count(),
        1
    );
}

#[test]
fn drawer_root_final_without_conversation_identity_fails_closed() {
    let workspace = WorkspaceId::new();
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::new(),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let operation = OperationId::new();
    let target = Target::Root(workspace);
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: None,
        operation_id: operation,
        profile: Some(AgentProfileId::new("codex").unwrap()),
    });
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Agent {
                operation,
                result: Ok(AgentPaneAdmission {
                    terminal: scoped_terminal_ref(workspace, None),
                    continuation: None,
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        Geometry { cols: 20, rows: 5 },
    );

    assert!(pending.is_empty());
    assert!(live_tab_terminals(&runtime, target).is_empty());
    assert!(durable.lock().unwrap().targets.is_empty());
    assert!(mutations.lock().unwrap().is_empty());
}

#[test]
fn select_tab_host_action_is_inert_without_an_active_target_or_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut pending = std::collections::HashMap::new();

    for mut runtime in [
        WorkspaceRuntime::new(workspace, Vec::new()),
        WorkspaceRuntime::new(workspace, vec![session]),
    ] {
        let session_ids = runtime.state().sessions().to_vec();
        let view_state = if session_ids.is_empty() {
            empty_state("demo")
        } else {
            state("demo")
        };
        let view = WorkspaceView::with_runtime_ids(ws("demo"), view_state, session_ids);
        let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::SelectTab(TabDirection::Next))
            .unwrap();
        drain_host_actions(&receiver, &mut ui, &mut runtime, &mut pending);
        assert!(runtime.focused_terminal().is_none());
    }
}

#[test]
fn compatibility_ports_fail_explicitly_and_never_silently_succeed() {
    struct DefaultSessionPort;
    impl SessionCommandPort for DefaultSessionPort {}

    let workspace_id = WorkspaceId::new();
    let session_id = SessionId::new();
    let workspace = ws("fallback");
    assert!(
        DefaultSessionPort
            .execute(&workspace, None, SessionCommand::List)
            .is_err()
    );
    assert!(
        UnavailableSessionCommandPort
            .execute(&workspace, None, SessionCommand::List)
            .is_err()
    );
    assert!(
        UnavailableAgentCommandPort
            .launch(OperationId::new(), workspace_id, None, None)
            .is_err()
    );
    assert!(
        UnavailableAgentCommandPort
            .launch_goal(OperationId::new(), workspace_id, None, "goal")
            .is_err()
    );
    // An embedder without a launch client refuses every pane launch inline
    // instead of leaving a pending tab forever.
    let history = interrupted_history(workspace_id, Some(session_id), true);
    assert!(
        UnavailablePaneLaunchPort
            .launch(OperationId::new(), workspace_id, None, None)
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .launch_goal(OperationId::new(), workspace_id, None, "goal")
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .resume(workspace_id, session_id, OperationId::new())
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .resume_exact(history.target.unwrap(), OperationId::new())
            .is_err()
    );
    assert!(
        UnavailablePaneLaunchPort
            .launch_terminal(
                workspace_id,
                None,
                terminal_geometry(20, 80),
                "new",
                OperationId::new(),
            )
            .is_err()
    );

    let decision_id = UserDecisionId::new();
    assert!(matches!(
        UnavailableDecisionCommandPort.refresh(workspace_id),
        BackendEvent::Notice(_)
    ));
    assert!(matches!(
        UnavailableDecisionCommandPort.resolve(
            workspace_id,
            decision_id,
            UserDecisionAnswer::Option {
                option_id: "safe".to_owned(),
            },
        ),
        BackendEvent::DecisionError { .. }
    ));
    assert!(matches!(
        UnavailableEnvironmentStore.load(EnvScope::Workspace),
        BackendEvent::EnvironmentError { .. }
    ));
    assert!(matches!(
        UnavailableEnvironmentStore.save(EnvScope::Global, Vec::new()),
        BackendEvent::EnvironmentError { .. }
    ));
    assert!(UnavailablePrSnapshotPort.snapshot(session_id).is_err());
    assert!(
        UnavailableBrowserOpener
            .open("https://example.com")
            .is_err()
    );
    NoDesktopNotifications.notify("title", "body");

    let mut settings = DefaultSettingsPort;
    settings
        .save(SettingsScope::Global, &Settings::default())
        .unwrap();
}

type SessionCommandCall = (String, Option<String>, SessionCommand);

struct RecordingExternalTerminalPort(Arc<Mutex<Vec<PathBuf>>>);

impl ExternalTerminalPort for RecordingExternalTerminalPort {
    fn open(&mut self, directory: &Path) -> Result<(), String> {
        self.0.lock().unwrap().push(directory.to_path_buf());
        Ok(())
    }
}

#[test]
fn unavailable_external_terminal_port_returns_a_safe_error() {
    assert_eq!(
        UnavailableExternalTerminalPort.open(Path::new("/tmp/worktree")),
        Err("external terminal launch is unavailable".to_owned())
    );
}

#[test]
fn external_terminal_launch_does_not_require_agent_port() {
    let workspace = WorkspaceId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), WorkspaceState::default(), Vec::new());
    let opened = Arc::new(Mutex::new(Vec::new()));
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_external_terminal(Box::new(RecordingExternalTerminalPort(opened.clone())));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::OpenExternalTerminal(Target::Root(
            workspace,
        )))
        .unwrap();

    drain_host_actions(
        &receiver,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert_eq!(*opened.lock().unwrap(), vec![PathBuf::from("/tmp/demo")]);
}

/// Bind a fake as the dedicated pane-launch client, the way the composition
/// root binds a second daemon client for launches. It is deliberately a
/// different instance from the resident stream port.
fn launch_port(port: Box<dyn AgentCommandPort>) -> Box<dyn PaneLaunchCommandPort> {
    Box::new(SerializedPaneLaunchPort::new(port))
}

struct SuccessfulAgentPort(TerminalRef);

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_screen_graph_fake_port_contract
impl AgentCommandPort for SuccessfulAgentPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Ok(AgentPaneAdmission {
            terminal: self.0.clone(),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn launch_goal(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _profile: Option<AgentProfileId>,
        _goal: &str,
    ) -> Result<AgentPaneAdmission, String> {
        Ok(AgentPaneAdmission {
            terminal: self.0.clone(),
            continuation: None,
            supervisor_run_id: None,
        })
    }
}

#[test]
fn serialized_launch_port_forwards_goal_admission() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let port = launch_port(Box::new(SuccessfulAgentPort(terminal.clone())));
    let admitted = port
        .launch_goal(OperationId::new(), workspace, None, "prepare a PR")
        .unwrap();
    assert!(admitted.terminal.fences(&terminal));
}

/// screen graph の workspace 遷移が実 port を通すことを検証する fake port。
/// `session create <name>` に対しては、daemon lifecycle snapshot を模して
/// `name` の session row を返し、sidebar への反映まで観測できるようにする。
#[derive(Clone)]
struct SnapshotSessionPort(Arc<Mutex<Vec<SessionCommandCall>>>);

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_screen_graph_fake_port_contract
impl SessionCommandPort for SnapshotSessionPort {
    fn execute(
        &self,
        workspace: &Workspace,
        selected: Option<&SessionRecord>,
        command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        let sessions = match &command {
            SessionCommand::Create { name, .. } => Some(vec![SessionRecord {
                name: name.clone(),
                display_name: None,
                origin: SessionOrigin::Human,
                started_from: None,
                root: workspace.path.join(".usagi/sessions").join(name),
                created_at: now(),
                last_active: None,
                notes: Scratchpad::default(),
                prs: Vec::new(),
            }]),
            SessionCommand::Remove { .. } => Some(Vec::new()),
            _ => None,
        };
        self.0.lock().unwrap().push((
            workspace.name.clone(),
            selected.map(|session| session.name.clone()),
            command,
        ));
        Ok(SessionCommandResult {
            message: "daemon accepted".to_owned(),
            sessions,
            session_ids: None,
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: None,
        })
    }
}

/// workspace 起動ごとに [`SnapshotSessionPort`] を新しく作る fake factory。
/// 記録した command 列と生成回数を共有し、全起動経路が実 port を fresh に
/// 通していることを固定する。
struct SnapshotSessionPortFactory {
    calls: Arc<Mutex<Vec<SessionCommandCall>>>,
    created: Arc<Mutex<usize>>,
}

impl SessionCommandPortFactory for SnapshotSessionPortFactory {
    fn create(&mut self) -> Box<dyn SessionCommandPort> {
        *self.created.lock().unwrap() += 1;
        Box::new(SnapshotSessionPort(self.calls.clone()))
    }
}

fn recent(name: &str) -> Recent {
    Recent::Workspace(WorkspaceOverview::new(ws(name), 1, 0, 0))
}

fn run(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    loader: &mut dyn WorkspaceLoader,
) -> io::Result<Exit> {
    run_from_start(term, workspaces, recent, now, Start::Welcome, loader)
}

#[test]
fn render_controller_frame_composites_the_home_and_overlays() {
    use crate::presentation::views::workspace::ProjectedSession;
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, Effect, Notice, OperationResult,
    };

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let projected = ProjectedSession {
        branch: "usagi/alpha".into(),
        id: session,
        label: "alpha".into(),
        detail: "fixture".into(),
        cwd: "/work/alpha".into(),
        last_modified: now(),
        has_notes: false,
        pr_count: 0,
        removing: false,
        agent_resume: None,
        lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
        failure_stage: None,
        failure_summary: None,
        role_id: None,
        parent_session_id: None,
        organization_depth: 0,
    };
    let sessions = std::slice::from_ref(&projected);
    let git = std::collections::BTreeMap::new();
    // Every case here composites the same Home geometry; only the runtime
    // and its session rows vary. Diagnostic health uses its unobserved
    // default so these assertions stay about the overlays.
    let frame = |runtime: &WorkspaceRuntime, sessions: &[ProjectedSession]| {
        render_controller_frame(
            20,
            80,
            runtime,
            "atlas",
            sessions,
            None,
            health(),
            &git,
            None,
            None,
        )
    };

    // Base Home frame: project identity stays in the outer tab bar, while
    // the Home frame renders its session row without a duplicate breadcrumb.
    let runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let base = frame(&runtime, sessions);
    assert!(!base.join("\n").contains("atlas"));
    assert!(base.join("\n").contains("alpha"));

    // Create form: with no sessions a single Down reaches + new session. It
    // renders inline in the sidebar row (the typed name), not as a centered
    // "New session" modal.
    let mut creating = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = creating.handle_key(Key::Down);
    let _ = creating.handle_key(Key::Enter);
    for character in ['b', 'e', 't', 'a'] {
        let _ = creating.handle_key(Key::Char(character));
    }
    let create = frame(&creating, &[]);
    assert!(create.join("\n").contains("beta"));
    assert!(!create.join("\n").contains("New session"));

    // Exit prompt overlay: the shared choice buttons and shortcut lines
    // render, defaulting to `quit` focused.
    let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = quitting.apply_event(AppEvent::Key(AppKey::CtrlQ));
    let quit = frame(&quitting, sessions);
    let quit_text = quit.join("\n");
    assert!(quit_text.contains("Leave this workspace?"));
    assert!(quit_text.contains("[ welcome ]"));
    assert!(quit_text.contains("[ quit    ]"));
    assert!(quit_text.contains("[ stay    ]"));
    assert!(quit_text.contains("←→/Tab: move"));

    // The runtime's persisted Overview palette renders through this path.
    let mut palette = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = palette.handle_key(Key::Char(':'));
    let overview = frame(&palette, sessions);
    assert!(overview.join("\n").contains("Overview"));

    let _ = palette.apply_event(AppEvent::Key(AppKey::SubmitOverview(
        "roles workspace".to_owned(),
    )));
    let _ = palette.apply_event(AppEvent::Backend(BackendEvent::RolesLoaded {
        scope: RoleEditorScope::Workspace,
        source: "version = 1\n".to_owned(),
    }));
    let roles = frame(&palette, sessions);
    assert!(roles.join("\n").contains("workspace roles.toml"));

    // Create-failure dialog: a failed create OperationResult opens it, and
    // this path composites the safe message over Home.
    let mut failing = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = failing.handle_key(Key::Down);
    let _ = failing.handle_key(Key::Enter);
    for character in ['a', 'p', 'i'] {
        let _ = failing.handle_key(Key::Char(character));
    }
    let token = match &failing.handle_key(Key::Enter)[..] {
        [Effect::CreateSession { token, .. }] => *token,
        other => panic!("expected a create effect, got {other:?}"),
    };
    let _ = failing.apply_event(AppEvent::OperationResult(OperationResult {
        token,
        succeeded: false,
        created: None,
        notice: Some(Notice::new("worktree path already exists")),
    }));
    let failure = frame(&failing, &[]);
    assert!(failure.join("\n").contains("Session create failed"));
    assert!(failure.join("\n").contains("worktree path already exists"));
}

#[test]
fn render_controller_frame_composites_terminal_launch_failure() {
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{AppEvent, Notice};

    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.apply_event(AppEvent::TerminalLaunchFailed(Notice::new(
        "login shell could not be started",
    )));

    let failure = render_controller_frame(
        20,
        80,
        &runtime,
        "atlas",
        &[],
        None,
        health(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .join("\n");
    assert!(failure.contains("Terminal failed to open"));
    assert!(failure.contains("login shell could not be started"));
}

#[test]
fn render_controller_frame_composites_agent_launch_failure() {
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{AppEvent, Notice};

    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.apply_event(AppEvent::AgentLaunchFailed(Notice::new(
        "agent process could not be started",
    )));

    let failure = render_controller_frame(
        20,
        80,
        &runtime,
        "atlas",
        &[],
        None,
        health(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .join("\n");
    assert!(failure.contains("Agent failed to start"));
    assert!(failure.contains("agent process could not be started"));
}

#[test]
fn workspace_shell_composition_keeps_home_below_the_project_bar() {
    let snapshot = snapshot("atlas");
    let deck = WorkspaceDeck::new(&snapshot);
    let home = (0..19)
        .map(|row| format!("home row {row}"))
        .collect::<Vec<_>>();

    let frame = compose_workspace_shell_frame(&deck, 19, 80, &home);

    assert_eq!(frame.len(), 20);
    assert!(strip_ansi(&frame[0]).contains("atlas"));
    assert_eq!(frame[1], "home row 0");
}

#[test]
fn closeup_environment_editor_is_composited_over_home() {
    use crate::presentation::views::workspace::ProjectedSession;
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{AppEvent, Effect};

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let _ = runtime.handle_key(Key::Enter);
    for _ in 0..3 {
        let _ = runtime.handle_key(Key::Down);
    }
    assert!(matches!(
        runtime.handle_key(Key::Enter).as_slice(),
        [Effect::LoadEnvironment {
            scope: EnvScope::Workspace
        }]
    ));
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::EnvironmentLoaded {
        scope: EnvScope::Workspace,
        entries: Vec::new(),
        inherited: Vec::new(),
    }));
    let sessions = [ProjectedSession {
        branch: "usagi/alpha".into(),
        id: session,
        label: "alpha".into(),
        detail: "fixture".into(),
        cwd: "/work/alpha".into(),
        last_modified: now(),
        has_notes: false,
        pr_count: 0,
        removing: false,
        agent_resume: None,
        lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
        failure_stage: None,
        failure_summary: None,
        role_id: None,
        parent_session_id: None,
        organization_depth: 0,
    }];
    let frame = render_controller_frame(
        20,
        80,
        &runtime,
        "atlas",
        &sessions,
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
    )
    .join("\n");
    assert!(frame.contains("Environment"));
    assert!(frame.contains("workspace env only (global values stay unchanged)"));
    assert!(frame.contains("one NAME=value binding per line"));
}

#[test]
fn unavailable_backend_reports_role_editor_errors_for_load_and_save() {
    use crate::usecase::application::daemon_backend::TargetStorePort as _;

    let mut port = UnavailableBackendPort;
    let (load, load_events) = Completions::channel();
    port.load_roles(RoleEditorScope::Workspace, load);
    assert!(matches!(
        load_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::RolesError {
            scope: RoleEditorScope::Workspace,
            ..
        })
    ));

    let (save, save_events) = Completions::channel();
    port.save_roles(RoleEditorScope::Global, "version = 1\n".to_owned(), save);
    assert!(matches!(
        save_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::RolesError {
            scope: RoleEditorScope::Global,
            ..
        })
    ));
}

#[test]
fn unavailable_backend_reports_pr_copy_and_dismiss_errors() {
    use crate::usecase::application::daemon_backend::OverlayPort as _;

    let mut port = UnavailableBackendPort;
    let (copy, copy_events) = Completions::channel();
    port.copy_pull_request("https://github.com/o/r/pull/7".to_owned(), copy);
    assert!(matches!(
        copy_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(_))
    ));

    let (dismiss, dismiss_events) = Completions::channel();
    port.dismiss_pull_request(
        SessionId::new(),
        "https://github.com/o/r/pull/7".to_owned(),
        dismiss,
    );
    assert!(matches!(
        dismiss_events.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(_))
    ));
}

#[test]
fn render_controller_frame_draws_a_waving_pending_create_skeleton() {
    // Once a create request is in flight, the shell threads its name here and
    // the sidebar draws a three-line loading skeleton just above `+ new
    // session` (document/03-tui.md). The sweep paints each cell with its own
    // SGR run, so compare on ANSI-stripped text.
    let strip = |frame: &[String]| {
        frame
            .iter()
            .map(|line| {
                let mut out = String::new();
                let mut chars = line.chars();
                while let Some(ch) = chars.next() {
                    if ch == '\u{1b}' {
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                                break;
                            }
                        }
                    } else {
                        out.push(ch);
                    }
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let workspace = WorkspaceId::new();
    let git = std::collections::BTreeMap::new();

    let idle = WorkspaceRuntime::new(workspace, Vec::new());
    let pending = render_controller_frame(
        20,
        80,
        &idle,
        "atlas",
        &[],
        None,
        health(),
        &git,
        None,
        Some("beta"),
    );
    let pending_text = strip(&pending);
    assert!(pending_text.contains("+ beta"));
    assert!(pending_text.contains("creating"));

    // No pending create means no skeleton or loading caption.
    let quiet = render_controller_frame(
        20,
        80,
        &idle,
        "atlas",
        &[],
        None,
        health(),
        &git,
        None,
        None,
    );
    let quiet_text = strip(&quiet);
    assert!(!quiet_text.contains("beta"));
    assert!(!quiet_text.contains("creating"));

    // The wave advances with the mascot tick rather than blinking statically.
    let mut ticked = WorkspaceRuntime::new(workspace, Vec::new());
    for _ in 0..12 {
        let _ = ticked.apply_event(AppEvent::Tick);
    }
    let pending_ticked = render_controller_frame(
        20,
        80,
        &ticked,
        "atlas",
        &[],
        None,
        health(),
        &git,
        None,
        Some("beta"),
    );
    assert_ne!(pending, pending_ticked);
}

#[test]
fn controller_loop_renders_home_and_detaches_on_quit_confirmation() {
    let snapshot = snapshot("demo");
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: snapshot.workspace_id,
        session_id: snapshot.session_ids.first().copied(),
        worktree_id: WorktreeId::new(),
    };
    // Ctrl-Q opens the quit confirmation; `y` detaches and ends the loop.
    let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, Key::Char('y')]);
    let result = run_workspace_controller(
        &mut term,
        snapshot,
        Box::new(UnavailableSessionCommandPort),
        Box::new(SuccessfulAgentPort(terminal.clone())),
        launch_port(Box::new(SuccessfulAgentPort(terminal))),
        Box::new(UnavailableDecisionCommandPort),
        Box::new(UnavailableEnvironmentStore),
        Box::new(NoDesktopNotifications),
        Box::new(NoMetrics),
        Box::new(UnavailablePrSnapshotPort),
        Box::new(UnavailableBrowserOpener),
    );

    assert!(matches!(result, Ok(Exit::Quit)));
    // The controller Home frame renders through render_home, and the quit
    // confirmation is composited before the loop detaches.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("demo"))
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Leave this workspace?"))
    );
    // Regression: the real Ctrl-Q frame carries the shared choice buttons and
    // the ←→/Tab shortcut, not the old free-text y/n prompt. Leaving and
    // quitting are separate buttons (#556).
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("[ quit    ]")),
        "exit prompt frame is missing the [ quit ] button"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("[ welcome ]")),
        "exit prompt frame is missing the [ welcome ] button"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("[ stay    ]")),
        "exit prompt frame is missing the [ stay ] button"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("←→/Tab: move")),
        "exit prompt frame is missing the move shortcut"
    );
}

struct BlockingRestorePort {
    entered: Sender<()>,
    release: Receiver<()>,
}

impl AgentCommandPort for BlockingRestorePort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("launch is unavailable".to_owned())
    }

    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        let _ = self.entered.send(());
        self.release
            .recv()
            .map_err(|_| TerminalError::Unavailable)?;
        Err(TerminalError::Unavailable)
    }
}

struct QuitWhileRestoreBlockedTerminal {
    entered: Option<Receiver<()>>,
    keys: VecDeque<Key>,
    frames: Vec<Vec<String>>,
}

impl Terminal for QuitWhileRestoreBlockedTerminal {
    fn size(&mut self) -> io::Result<(usize, usize)> {
        Ok((20, 80))
    }

    fn draw(&mut self, frame: &[String]) -> io::Result<()> {
        self.frames.push(frame.to_vec());
        Ok(())
    }

    fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
        Ok(())
    }

    fn read_key(&mut self) -> io::Result<Key> {
        if let Some(entered) = self.entered.take() {
            entered
                .recv_timeout(std::time::Duration::from_secs(1))
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        self.keys
            .pop_front()
            .ok_or_else(|| io::Error::other("no more keys"))
    }
}

#[test]
fn blocked_restore_inventory_never_blocks_render_or_quit() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut term = QuitWhileRestoreBlockedTerminal {
        entered: Some(entered_rx),
        keys: VecDeque::from([Key::CtrlQ, Key::Char('y')]),
        frames: Vec::new(),
    };
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: Some(Box::new(BlockingRestorePort {
            entered: entered_tx,
            release: release_rx,
        })),
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };

    let started = std::time::Instant::now();
    let result =
        run_workspace_controller_with_backend(&mut term, snapshot("blocked-restore"), &mut factory);
    let elapsed = started.elapsed();
    let _ = release_tx.send(());

    assert_eq!(result.unwrap(), Exit::Quit);
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "quit waited for a blocked restore worker: {elapsed:?}"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("blocked-restore"))
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| { frame.join("\n").contains("Leave this workspace?") })
    );
}

#[test]
fn direct_controller_entry_uses_the_resolved_workspace_settings() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char(':'),
        Key::Char('i'),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut factory = FixedBackendFactory {
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
    let settings = usagi_core::domain::settings::Settings {
        modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode::Prompt,
        ..usagi_core::domain::settings::Settings::default()
    };

    assert_eq!(
        run_workspace_controller_with_backend_and_settings(
            &mut term,
            snapshot("direct"),
            &mut factory,
            &settings,
        )
        .unwrap(),
        Exit::Quit
    );
    assert!(term.frames.iter().any(|frame| {
        let frame = frame.join("\n");
        frame.contains("Overview") && frame.contains("Enter: run   Esc: close")
    }));
}

/// #551 acceptance. The frame loop must be "non-blocking drain → projection
/// → draw → input" and nothing else: neither a wake-up tick nor a resize may
/// reach a daemon lane, and no frame may spawn a session worker. Both used
/// to happen on every `Key::Other`, at 62.5Hz.
#[test]
fn ticks_and_resizes_never_reach_a_daemon_lane_or_spawn_a_session_worker() {
    let decision_wakes = Arc::new(AtomicUsize::new(0));
    let decision_polls = Arc::new(AtomicUsize::new(0));
    let lane_wakes = Arc::new(AtomicUsize::new(0));
    let lane_drains = Arc::new(AtomicUsize::new(0));
    let session_calls = Arc::new(Mutex::new(Vec::new()));

    // Forty wake-ups interleaved with forty resizes — the shape of dragging
    // a window edge while nothing else happens — then a modal open/close and
    // quit, all while both lanes stay silent.
    let mut keys = Vec::new();
    for _ in 0..40 {
        keys.push(Key::Other);
        keys.push(Key::Resize);
    }
    keys.extend([
        Key::Char(':'),
        Key::Char('i'),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(SnapshotSessionPort(Arc::clone(&session_calls)))),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: Some(Box::new(FakeSessionRefreshPort {
            wakes: Arc::clone(&lane_wakes),
            takes: Arc::clone(&lane_drains),
            queued: Arc::default(),
        })),
        decisions: Some(Box::new(CountingDecisionPort {
            wakes: Arc::clone(&decision_wakes),
            polls: Arc::clone(&decision_polls),
        })),
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot("idle"), &mut factory).unwrap(),
        Exit::Quit
    );

    // One seed wake per lane for the whole run — not one per frame.
    assert_eq!(decision_wakes.load(Ordering::SeqCst), 1);
    assert_eq!(lane_wakes.load(Ordering::SeqCst), 1);
    // The command port, and therefore `std::thread::spawn`, is untouched:
    // the tick no longer runs `SessionCommand::List`.
    assert!(
        session_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
    // What the loop does do every frame is drain. Since #554 the redraw is
    // gated on the frame's material, so a tick that changes nothing draws
    // nothing — the per-iteration invariant lives in the drain counts, not
    // in the frame count.
    assert!(decision_polls.load(Ordering::SeqCst) >= 80);
    assert!(lane_drains.load(Ordering::SeqCst) >= 80);
    // Draw, modal, and quit all completed with both lanes never answering.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Overview"))
    );
}

/// A worktree scan that counts how often the frame loop reaches the disk.
struct CountingWorktreeScanPort {
    scans: Arc<AtomicUsize>,
    names: Vec<String>,
}

impl SessionWorktreeScanPort for CountingWorktreeScanPort {
    fn scan(&mut self, _workspace: &std::path::Path) -> Vec<String> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        self.names.clone()
    }
}

fn counting_scan(scans: &Arc<AtomicUsize>) -> Box<dyn SessionWorktreeScanPort> {
    Box::new(CountingWorktreeScanPort {
        scans: Arc::clone(scans),
        names: vec!["stale-worktree".to_owned()],
    })
}

/// 16ms is the composition root's tick period, so this is "frame `tick`".
fn at_tick(tick: u64) -> std::time::Duration {
    std::time::Duration::from_millis(16 * tick)
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

/// #554 acceptance. An open form scans immediately — so the very first frame
/// that shows the caret can already reject a known collision — and then no
/// more than once per cadence period however long it stays open.
#[test]
fn an_open_create_form_scans_on_open_and_then_at_the_cadence_ceiling() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut hint = SessionWorktreeHint::new(counting_scan(&scans));
    let workspace = std::path::Path::new("/tmp/demo");

    // The frame that opens the form.
    assert_eq!(hint.names(true, workspace, at_tick(0)), ["stale-worktree"]);
    assert_eq!(scans.load(Ordering::SeqCst), 1);

    // Five seconds of 16ms ticks with the form left open.
    let ticks = 313;
    for tick in 1..ticks {
        assert_eq!(
            hint.names(true, workspace, at_tick(tick)),
            ["stale-worktree"]
        );
    }

    let elapsed = at_tick(ticks - 1);
    let ceiling =
        usize::try_from(1 + elapsed.as_millis() / SessionWorktreeHint::CADENCE.as_millis())
            .expect("the ceiling of a five second run fits a usize");
    let scanned = scans.load(Ordering::SeqCst);
    assert!(
        scanned <= ceiling,
        "{scanned} scans over {elapsed:?} exceeds the cadence ceiling of {ceiling}"
    );
    assert_eq!(scanned, 10);
}

/// Reopening the form is the moment the hint matters most, so it always
/// rescans — even when the previous scan is still inside the cadence window.
#[test]
fn reopening_the_create_form_rescans_inside_the_cadence_window() {
    let scans = Arc::new(AtomicUsize::new(0));
    let mut hint = SessionWorktreeHint::new(counting_scan(&scans));
    let workspace = std::path::Path::new("/tmp/demo");

    assert_eq!(hint.names(true, workspace, at_tick(0)), ["stale-worktree"]);
    assert_eq!(hint.names(true, workspace, at_tick(1)), ["stale-worktree"]);
    assert_eq!(scans.load(Ordering::SeqCst), 1);

    assert!(hint.names(false, workspace, at_tick(2)).is_empty());
    assert_eq!(hint.names(true, workspace, at_tick(3)), ["stale-worktree"]);
    assert_eq!(scans.load(Ordering::SeqCst), 2);
}

/// #554 acceptance, through the real frame loop: an idle Home reaches
/// neither the filesystem nor the renderer on a tick that changes nothing,
/// while every drain still runs on exactly those ticks.
#[test]
fn idle_ticks_skip_the_worktree_scan_and_the_redraw_but_never_a_drain() {
    reset_projection_build_counts();
    let scans = Arc::new(AtomicUsize::new(0));
    let lane_drains = Arc::new(AtomicUsize::new(0));

    let ticks = 1_000;
    let mut keys = vec![Key::Other; ticks];
    keys.extend([Key::CtrlQ, Key::Char('y')]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: Some(Box::new(FakeSessionRefreshPort {
            wakes: Arc::default(),
            takes: Arc::clone(&lane_drains),
            queued: Arc::default(),
        })),
        decisions: None,
        session_worktrees: Some(counting_scan(&scans)),
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot("idle"), &mut factory).unwrap(),
        Exit::Quit
    );

    assert_eq!(
        scans.load(Ordering::SeqCst),
        0,
        "an idle frame reached the sessions directory"
    );
    let frames = term.frames.len();
    assert!(
        frames < ticks,
        "{frames} draws for {ticks} ticks: the redraw gate did nothing"
    );
    // The floor is the rabbit: it has three distinct appearances per six
    // ticks and #554 keeps that cadence, so roughly half the idle ticks are
    // genuinely material.
    assert!(
        frames <= ticks / 2 + 4,
        "{frames} draws for {ticks} ticks is above the animation floor"
    );
    // Every iteration still drained the resident lane, including the ones
    // that drew nothing.
    assert!(lane_drains.load(Ordering::SeqCst) >= ticks);
    let (session_builds, terminal_builds) = projection_build_counts();
    assert_eq!(
        session_builds, 1,
        "session rows/cwd were rebuilt on idle ticks"
    );
    assert_eq!(
        terminal_builds, 1,
        "terminal viewport/link projection was rebuilt on idle ticks"
    );
}

/// A terminal harness that records the projection generations visible at
/// every actual draw. With `wait_for_builds`, it drives neutral ticks until
/// the requested cache invalidation has happened, then quits. The condition
/// is observable loop state rather than elapsed time; the finite ceiling is
/// only a failure guard for a broken wiring under test.
struct CacheInvalidationTerminal {
    keys: VecDeque<Key>,
    wait_for_builds: Option<(usize, usize)>,
    quit_started: bool,
    neutral_ticks: usize,
    frames: Vec<Vec<String>>,
    builds_at_draw: Vec<(usize, usize)>,
}

impl CacheInvalidationTerminal {
    fn scripted(keys: impl IntoIterator<Item = Key>) -> Self {
        Self {
            keys: keys.into_iter().collect(),
            wait_for_builds: None,
            quit_started: false,
            neutral_ticks: 0,
            frames: Vec::new(),
            builds_at_draw: Vec::new(),
        }
    }

    fn until_builds(keys: impl IntoIterator<Item = Key>, builds: (usize, usize)) -> Self {
        Self {
            wait_for_builds: Some(builds),
            ..Self::scripted(keys)
        }
    }
}

impl Terminal for CacheInvalidationTerminal {
    fn size(&mut self) -> io::Result<(usize, usize)> {
        Ok((20, 80))
    }

    fn draw(&mut self, frame: &[String]) -> io::Result<()> {
        self.frames.push(frame.to_vec());
        self.builds_at_draw.push(projection_build_counts());
        Ok(())
    }

    fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
        Ok(())
    }

    fn read_key(&mut self) -> io::Result<Key> {
        if let Some(key) = self.keys.pop_front() {
            return Ok(key);
        }
        let Some(expected) = self.wait_for_builds else {
            return Err(io::Error::other("no more cache-invalidation keys"));
        };
        let observed = projection_build_counts();
        if observed.0 >= expected.0 && observed.1 >= expected.1 {
            if self.quit_started {
                return Ok(Key::Char('y'));
            }
            self.quit_started = true;
            return Ok(Key::Live(LiveTerminalAction::QuitConfirmation));
        }
        self.neutral_ticks += 1;
        if self.neutral_ticks >= 10_000 {
            return Err(io::Error::other(format!(
                "cache invalidation was not observed: expected {expected:?}, got {observed:?}"
            )));
        }
        std::thread::yield_now();
        Ok(Key::Other)
    }

    fn copy_text(&mut self, _text: &str) -> Result<(), String> {
        Ok(())
    }
}

/// The session revision must cross both cache gates in the real composition
/// loop: first rebuild the owned row/path projection, then rebuild and draw
/// the frame that contains it.
#[test]
fn daemon_session_change_invalidates_the_joined_material_and_redraws() {
    reset_projection_build_counts();
    let snapshot = snapshot("session-cache");
    let original = snapshot.session_ids[0];
    let added = SessionId::new();
    let mut added_record = snapshot.state.sessions[0].clone();
    added_record.name = "cache-added".to_owned();
    added_record.root = PathBuf::from("/tmp/session-cache/cache-added");
    let update = SessionCommandResult {
        message: "daemon snapshot changed".to_owned(),
        sessions: Some(vec![snapshot.state.sessions[0].clone(), added_record]),
        session_ids: Some(vec![original, added]),
        agent_resumes: None,
        session_lifecycles: None,
        session_roles: None,
        revision: Some(1),
    };
    let mut term = CacheInvalidationTerminal::scripted([
        Key::Other,
        Key::Other,
        Key::Other,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: Some(Box::new(ScheduledSessionRefreshPort {
            publish_on_take: 3,
            takes: 0,
            update: Some(update),
        })),
        decisions: None,
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
        Exit::Quit
    );

    let (session_builds, terminal_builds) = projection_build_counts();
    assert_eq!(session_builds, 2, "the changed session key did not rebuild");
    assert_eq!(terminal_builds, 1, "a session change rebuilt the terminal");
    assert!(
        term.builds_at_draw.contains(&(2, 1)),
        "the frame key did not redraw after the session material rebuild"
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("cache-added")),
        "the redrawn frame did not contain the changed session projection"
    );
}

struct ImmediateTerminalLaunchPort(TerminalRef);

impl PaneLaunchCommandPort for ImmediateTerminalLaunchPort {
    fn launch(
        &self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("agent launch is not scripted".to_owned())
    }

    fn resume(
        &self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("agent resume is not scripted".to_owned())
    }

    fn resume_exact(
        &self,
        _target: usagi_core::domain::agent::AgentResumeTarget,
        _operation: OperationId,
    ) -> Result<super::ExactAgentResume, String> {
        Err("exact agent resume is not scripted".to_owned())
    }

    fn launch_terminal(
        &self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        _operation: OperationId,
    ) -> Result<TerminalRef, String> {
        Ok(self.0.clone())
    }
}

/// The resident stream starts with one checkpoint and publishes exactly one
/// later output chunk. This changes the authoritative `TerminalSession` screen
/// revision after the pane itself has already been projected once.
struct ChangingTerminalPort {
    replay: Vec<u8>,
    empty_polls_before_update: usize,
    update: Option<Vec<u8>>,
}

impl AgentCommandPort for ChangingTerminalPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("agent launch is not scripted".to_owned())
    }

    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Ok(TerminalAttach {
            subscription: TerminalSubscription { id: 1, epoch: 1 },
            revision: 1,
            output_offset: u64::try_from(self.replay.len()).expect("test replay fits in u64"),
            next_input_seq: None,
            screen: attach_checkpoint(&self.replay, geometry),
            exited: false,
        })
    }

    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        if self.empty_polls_before_update > 0 {
            self.empty_polls_before_update -= 1;
            return Ok(Vec::new());
        }
        let Some(data) = self.update.take() else {
            return Ok(Vec::new());
        };
        Ok(vec![TerminalChunk {
            start_offset: after_offset,
            end_offset: after_offset + data.len() as u64,
            data,
        }])
    }
}

/// The actual controller path must propagate both a focused-pane change and
/// a later terminal screen revision through `terminal_material_key` and the
/// aggregate `FrameMaterialKey`. Each invalidation owes one viewport/link
/// rebuild and one draw containing the new owned projection.
#[test]
fn terminal_output_change_invalidates_the_joined_material_and_redraws() {
    reset_projection_build_counts();
    let snapshot = snapshot("terminal-cache");
    let terminal = live_terminal_ref(snapshot.workspace_id, snapshot.session_ids[0]);
    let mut keys = vec![Key::Enter, Key::Live(LiveTerminalAction::OpenCloseupModal)];
    keys.extend("terminal open".chars().map(Key::Char));
    keys.push(Key::Enter);
    let mut term = CacheInvalidationTerminal::until_builds(keys, (1, 3));
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(ChangingTerminalPort {
            replay: b"cache-before".to_vec(),
            empty_polls_before_update: 1,
            update: Some(b"\r\ncache-after".to_vec()),
        })),
        launch: Some(Box::new(ImmediateTerminalLaunchPort(terminal))),
        restore: None,
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot, &mut factory).unwrap(),
        Exit::Quit
    );

    let (session_builds, terminal_builds) = projection_build_counts();
    assert_eq!(session_builds, 1, "a terminal change rebuilt session rows");
    assert_eq!(
        terminal_builds, 3,
        "focused pane and screen revision did not each invalidate the terminal key"
    );
    for generation in [2, 3] {
        assert!(
            term.builds_at_draw.contains(&(1, generation)),
            "frame key did not redraw terminal generation {generation}"
        );
    }
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("cache-after")),
        "the redraw did not contain output from the changed terminal screen"
    );
}

/// One admitted restore job and what the frame gate did on the tick that
/// admitted it.
#[derive(Debug)]
struct RestoreAdmission {
    /// Each job runs on its own thread, so the thread id is what separates
    /// two admissions from the several inventory calls inside one of them.
    job: std::thread::ThreadId,
    /// Frames the terminal had drawn when this job started.
    drawn: usize,
    /// The admitting tick drew nothing: the frame count had not moved since
    /// that tick began.
    skipped: bool,
}

/// What the #554 skipped-tick acceptance observes. The frame gate runs on the
/// loop thread and the admission is observed from the worker thread the job
/// runs on, so both counts are shared: an admission is on a skipped tick
/// when the frame count has not moved since that tick started.
#[derive(Default)]
struct RestoreAdmissionLog {
    /// Frames the terminal was asked to draw.
    draws: AtomicUsize,
    /// `draws` as of the start of the tick now running. The terminal
    /// republishes it when a tick ends, which is when it reads the next key.
    draws_at_tick_start: AtomicUsize,
    /// Inventory calls the admitted jobs have made. A job that has stopped
    /// calling is a job that has finished.
    calls: AtomicUsize,
    admissions: Mutex<Vec<RestoreAdmission>>,
}

impl RestoreAdmissionLog {
    fn drew(&self) {
        self.draws.fetch_add(1, Ordering::SeqCst);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Close the current tick. Called from the terminal's `read_key`, the
    /// last thing a loop iteration does.
    fn tick_ended(&self) {
        self.draws_at_tick_start
            .store(self.draws.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Record one inventory call. A job's first call is its admission, and
    /// every call is what the driver watches to know the job is still
    /// running.
    fn admitted(&self, job: std::thread::ThreadId) {
        let tick_start = self.draws_at_tick_start.load(Ordering::SeqCst);
        let drawn = self.draws.load(Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut admissions = self.admitted_jobs();
        if admissions.last().map(|admission| admission.job) != Some(job) {
            admissions.push(RestoreAdmission {
                job,
                drawn,
                skipped: drawn == tick_start,
            });
        }
    }

    /// A retry — an admission after the first — ran on a tick that drew
    /// nothing. This is the frame-skip contract the loop is driven until it
    /// observes.
    fn retry_admitted_on_a_skipped_tick(&self) -> bool {
        self.admitted_jobs()
            .iter()
            .skip(1)
            .any(|admission| admission.skipped)
    }

    fn admitted_jobs(&self) -> std::sync::MutexGuard<'_, Vec<RestoreAdmission>> {
        self.admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A restore client that always fails and records, once per admitted job,
/// what the frame gate did on the tick that admitted it.
struct AdmissionCountingRestorePort {
    log: Arc<RestoreAdmissionLog>,
}

impl AgentCommandPort for AdmissionCountingRestorePort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("launch is unavailable".to_owned())
    }

    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        self.log.admitted(std::thread::current().id());
        Err(TerminalError::Unavailable)
    }
}

/// Ticks the retry acceptance drives before giving up. The retry it waits for
/// comes due after one backoff step (250ms), so this is a wide bound whose
/// only job is to end a run that observes nothing in its assertions instead
/// of driving forever.
const MAX_DRIVEN_RETRY_TICKS: usize = 200;

/// Ticks without a new inventory call the driver waits for before quitting.
/// The observed job is admitted on its first call and sleeps between its
/// three attempts, so quiet ticks are how the driver knows it has finished.
/// Returning from the loop mid-job would leave that worker running into the
/// rest of the suite, where it keeps writing `spawn_restore_job`'s coverage
/// counters while the harness reads them — enough to make a line another
/// test covers report as uncovered. The next retry is a 500ms backoff step
/// away, so this quiet window stays inside the gap and admits nothing new.
const RETRY_QUIET_TICKS: usize = 2;

/// A terminal that paces the loop so wall time advances between frames and
/// keeps it running with an inert key until the admission log holds the
/// observation the test needs, then quits. Driving to the observation is what
/// makes the assertion deterministic: which tick skips its frame depends on
/// when the material last changed, so no fixed key script can promise that a
/// retry lands on a skipped tick (#567).
struct RetryDrivingTerminal {
    log: Arc<RestoreAdmissionLog>,
    pace: std::time::Duration,
    /// Ticks driven so far, bounded by [`MAX_DRIVEN_RETRY_TICKS`].
    ticks: usize,
    /// Inventory calls seen at the previous tick, and how many ticks have
    /// passed without a new one.
    calls: usize,
    quiet_ticks: usize,
    /// The quit sequence, queued once the loop is done driving.
    quit: VecDeque<Key>,
}

impl Terminal for RetryDrivingTerminal {
    fn size(&mut self) -> io::Result<(usize, usize)> {
        Ok((20, 80))
    }

    fn draw(&mut self, _frame: &[String]) -> io::Result<()> {
        self.log.drew();
        Ok(())
    }

    fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
        Ok(())
    }

    fn read_key(&mut self) -> io::Result<Key> {
        // The last call of a loop iteration, so the frame count from here is
        // the one the next tick starts from.
        self.log.tick_ended();
        if let Some(key) = self.quit.pop_front() {
            return Ok(key);
        }
        let calls = self.log.calls();
        self.quiet_ticks = if calls == self.calls {
            self.quiet_ticks + 1
        } else {
            self.calls = calls;
            0
        };
        let settled =
            self.quiet_ticks >= RETRY_QUIET_TICKS && self.log.retry_admitted_on_a_skipped_tick();
        if settled || self.ticks >= MAX_DRIVEN_RETRY_TICKS {
            self.quit.push_back(Key::Char('y'));
            return Ok(Key::CtrlQ);
        }
        self.ticks += 1;
        // Wall time has to advance for the retry backoff to come due.
        std::thread::sleep(self.pace);
        // `Escape` is inert on the base Switch route, so driving with it
        // leaves the frame's material unchanged.
        Ok(Key::Escape)
    }
}

/// Keep the short retry test away from the minute boundary that materially
/// updates relative-time labels. Most runs return immediately; a run in the
/// last ten seconds of a minute parks until the next one begins.
fn wait_for_a_stable_relative_time_minute() {
    let now = Utc::now();
    if now.second() < 50 {
        return;
    }
    let remaining = std::time::Duration::from_secs(60 - u64::from(now.second()))
        .saturating_sub(std::time::Duration::from_nanos(u64::from(now.nanosecond())));
    std::thread::sleep(remaining);
}

/// #554 acceptance. The skip covers the drawing and nothing else: the
/// restore retry's admission sits after the gate and must still fire on a
/// tick that drew nothing. The loop is driven until a retry is admitted on
/// such a tick, so the assertion never rests on a run where every tick
/// happened to be material (#567).
#[test]
fn a_skipped_tick_still_admits_the_restore_retry() {
    wait_for_a_stable_relative_time_minute();
    let log = Arc::new(RestoreAdmissionLog::default());
    let mut term = RetryDrivingTerminal {
        log: Arc::clone(&log),
        pace: std::time::Duration::from_millis(60),
        ticks: 0,
        calls: 0,
        quiet_ticks: 0,
        quit: VecDeque::new(),
    };
    let mut factory = FixedBackendFactory {
        sessions: Some(Box::new(UnavailableSessionCommandPort)),
        agent: Some(Box::new(UnavailableAgentCommandPort)),
        launch: None,
        restore: Some(Box::new(AdmissionCountingRestorePort {
            log: Arc::clone(&log),
        })),
        metrics: Some(Box::new(NoMetrics)),
        browser: Some(Box::new(UnavailableBrowserOpener)),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };

    assert_eq!(
        run_workspace_controller_with_backend(&mut term, snapshot("retry"), &mut factory).unwrap(),
        Exit::Quit
    );

    let admissions = log.admitted_jobs();
    let driven = term.ticks;
    let drawn_at: Vec<usize> = admissions.iter().map(|admission| admission.drawn).collect();
    let admitted = drawn_at.len();
    assert!(
        admitted >= 2,
        "the restore retry was admitted {admitted} time(s) in {driven} tick(s)"
    );
    // A retry admitted on a skipped tick is the whole contract, so a run that
    // never skipped one fails here instead of reading as a pass.
    let retry_on_a_skipped_tick = admissions.iter().skip(1).any(|admission| admission.skipped);
    assert!(
        retry_on_a_skipped_tick,
        "every restore retry followed a redraw in {driven} tick(s), \
             admitted at frame {drawn_at:?}"
    );
}

/// #554 acceptance. Skipping is decided by comparing the renderer's inputs,
/// so this pins each of those inputs: change one and the frame must differ,
/// change none and it must not — including across the ticks the rabbit
/// spends resting.
#[test]
#[allow(clippy::too_many_lines)] // One arm per material the renderer reads; splitting hides the table.
fn the_frame_material_changes_for_every_input_the_renderer_reads() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let record = SessionRecord {
        name: "alpha".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/alpha"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let sessions = vec![ProjectedSession::from_record(session, &record)];
    let no_diffs = BTreeMap::new();
    let clock = super::relative_time_clock(now()) + Duration::seconds(10);
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

    let material = |runtime: &WorkspaceRuntime| {
        home_frame_material(
            20,
            80,
            runtime,
            "demo",
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            clock,
        )
    };
    let base = material(&runtime);

    // The decorative rabbit holds its resting pose for 32 runtime ticks, so
    // an idle Home does not repaint at the terminal clock cadence.
    for _ in 0..31 {
        let _ = runtime.apply_event(AppEvent::Tick);
        assert_eq!(material(&runtime), base, "a resting tick forced a redraw");
    }
    // The blink and the ear flop are each held for eight ticks, but both
    // transitions must still reach the terminal.
    let _ = runtime.apply_event(AppEvent::Tick);
    let blink = material(&runtime);
    assert_ne!(blink, base, "the rabbit stopped blinking");
    for _ in 0..8 {
        let _ = runtime.apply_event(AppEvent::Tick);
    }
    let flop = material(&runtime);
    assert_ne!(flop, blink, "the rabbit stopped flopping its ear");
    for _ in 0..8 {
        let _ = runtime.apply_event(AppEvent::Tick);
    }
    assert_eq!(
        material(&runtime),
        base,
        "the rabbit never came back to rest"
    );

    // Terminal size (a resize that actually changes the geometry).
    let resized = home_frame_material(
        21,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock,
    );
    assert_ne!(resized, base, "a resize did not redraw");

    // The wall clock behind relative session times is independent from the
    // monotonic animation clock. Neither sub-second nor one-second changes
    // rebuild an ordinary Home; the next minute does.
    let sub_second = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock + Duration::milliseconds(400),
    );
    assert_eq!(sub_second, base, "sub-second jitter forced a redraw");
    let next_second = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock + Duration::seconds(1),
    );
    assert_eq!(next_second, base, "a wall-clock second forced a redraw");
    let next_minute = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        None,
        clock + Duration::minutes(1),
    );
    assert_ne!(next_minute, base, "the relative session times froze");

    // Daemon metrics for the mascot sidecar.
    let metrics = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        StaticMetrics.latest(),
        health(),
        &no_diffs,
        None,
        None,
        clock,
    );
    assert_ne!(metrics, base, "a metrics update did not redraw");

    // The diagnostic health observer. It is a renderer input, so it belongs
    // to the material: a newly observed sample must be able to change the
    // sidecar, and an unchanged observer must not force a redraw.
    let mut observed = DaemonHealthTracker::default();
    observed.observe(&StaticMetrics.latest().expect("static metrics"));
    let health_material = home_frame_material(
        20, 80, &runtime, "demo", &sessions, None, observed, &no_diffs, None, None, clock,
    );
    assert_ne!(health_material, base, "a health observation did not redraw");

    // Git diffs joined onto the sidebar rows.
    let diffs = BTreeMap::from([(
        session,
        GitDiff {
            base: "main".to_owned(),
            ahead: 1,
            behind: 0,
            added: 1,
            removed: 2,
        },
    )]);
    let git = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &diffs,
        None,
        None,
        clock,
    );
    assert_ne!(git, base, "a git diff update did not redraw");

    // Live terminal output.
    let view = TerminalViewProjection {
        rows: vec!["output".to_owned()],
        row_offset: 0,
        total_rows: 1,
        scroll: 0,
        feedback: None,
    };
    let terminal_output = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        Some(view.clone()),
        None,
        clock,
    );
    assert_ne!(terminal_output, base, "terminal output did not redraw");

    let mut root_drawer = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = root_drawer.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let root_output = home_frame_material(
        20,
        80,
        &root_drawer,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        Some(view.clone()),
        None,
        clock,
    );
    assert!(root_output.projection.terminal_view().is_none());
    assert_eq!(
        render_home_material(&root_output)
            .join("\n")
            .matches("output")
            .count(),
        1
    );

    // The pending create skeleton.
    let pending = home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &sessions,
        None,
        health(),
        &no_diffs,
        None,
        Some("beta"),
        clock,
    );
    assert_ne!(pending, base, "a pending create did not redraw");

    // Reducer state, and the two overlays composited outside `render_home`.
    let mut moved = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = moved.handle_key(Key::Down);
    assert_ne!(material(&moved), base, "a selection move did not redraw");

    let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = quitting.handle_key(Key::CtrlQ);
    let confirming = material(&quitting);
    assert_ne!(confirming, base, "the quit confirmation did not redraw");
    let _ = quitting.handle_key(Key::Left);
    assert_ne!(
        material(&quitting),
        confirming,
        "moving the quit confirmation's focus did not redraw"
    );
}

#[test]
fn garden_motion_uses_the_logical_clock_inside_one_relative_time_minute() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let record = SessionRecord {
        name: "alpha".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/alpha"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let sessions = vec![ProjectedSession::from_record(session, &record)];
    let no_diffs = BTreeMap::new();
    let clock = super::relative_time_clock(now()) + Duration::seconds(10);
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let runtime_id =
        AgentRuntimeId::parse("00000000-0000-4000-8000-000000000001").expect("fixture runtime id");
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
    };
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::RuntimePhase {
        runtime: AgentRuntimeRef {
            agent_runtime_id: runtime_id,
            terminal,
            session_id: Some(session),
        },
        phase: usagi_core::domain::session_lifecycle::AgentPhase::Running,
    }));
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let material = |runtime: &WorkspaceRuntime, wall_clock| {
        home_frame_material(
            24,
            100,
            runtime,
            "demo",
            &sessions,
            None,
            health(),
            &no_diffs,
            None,
            None,
            wall_clock,
        )
    };

    let first = material(&runtime, clock);
    assert!(first.projection.garden_sessions().is_some());
    for _ in 0..7 {
        let _ = runtime.apply_event(AppEvent::Tick);
    }
    assert_eq!(
        material(&runtime, clock + Duration::milliseconds(900)),
        first,
        "the Garden rebuilt at the 16 ms input-pump cadence"
    );

    let _ = runtime.apply_event(AppEvent::Tick);
    let advanced = material(&runtime, clock + Duration::seconds(1));
    assert_eq!(
        first.now, advanced.now,
        "relative-time clock left its minute"
    );
    assert_ne!(
        first.projection.garden_animation_tick(),
        advanced.projection.garden_animation_tick(),
        "the canonical Garden pose did not advance"
    );
    assert_ne!(
        render_home_material(&first),
        render_home_material(&advanced),
        "the running pose froze inside one wall-clock minute"
    );
}

/// #554 acceptance for the entry screens. They have no clock and no
/// background lane, so an idle Welcome must draw exactly once however long
/// the terminal keeps ticking.
#[test]
fn idle_entry_screen_ticks_draw_nothing_after_the_first_frame() {
    let mut keys = vec![Key::Other; 60];
    keys.push(Key::Char('q'));
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(
        term.frames.len(),
        1,
        "an idle Welcome rebuilt its frame on a tick"
    );
}

/// The entry gate still redraws the moment the form or the screen changes,
/// so input latency is unaffected: every tick that carries a change draws,
/// and only the empty ones in between are skipped.
#[test]
fn entry_screen_input_redraws_immediately() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Other,
        Key::Down,
        Key::Other,
        Key::Enter,
        Key::Other,
        Key::Escape,
        Key::Other,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    // Welcome, the selection move, the screen it opens, and Welcome again:
    // one frame per input that changed something, none for the four
    // interleaved ticks.
    assert_eq!(term.frames.len(), 4);
}

/// #551 acceptance: several `RefreshSessions` inside one cadence period are
/// answered by the one snapshot the lane publishes, and a lane that never
/// answers parks at most one completion instead of accumulating them.
#[test]
fn refresh_requests_coalesce_onto_one_published_snapshot() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut pending_targets = std::collections::HashMap::new();
    let mut pending_refresh = None;
    let (host, actions) = ControllerHost::channel();
    let mut backend = DaemonBackend::new(
        Box::new(host.clone()),
        Box::new(host),
        Box::new(UnavailableBackendPort),
        Box::new(UnavailableBackendPort),
    );
    let wakes = Arc::new(AtomicUsize::new(0));
    let published = SessionId::new();
    let mut lane = FakeSessionRefreshPort {
        wakes: Arc::clone(&wakes),
        takes: Arc::default(),
        queued: Arc::new(Mutex::new(VecDeque::from([Ok(SessionCommandResult {
            message: "daemon snapshot refreshed".to_owned(),
            sessions: Some(ui.workspace.sessions().to_vec()),
            session_ids: Some(vec![published]),
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: Some(7),
        })]))),
    };

    for _ in 0..3 {
        backend.dispatch(Effect::RefreshSessions { workspace });
    }
    super::drain_controller_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut pending_targets,
        &mut lane,
        &mut pending_refresh,
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 3);
    assert!(pending_refresh.is_some());

    super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(pending_refresh.is_none());
    assert_eq!(ui.workspace.session_ids(), &[published]);
    assert_eq!(ui.last_session_revision, 7);
    let events = backend.drain_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        AppEvent::Backend(BackendEvent::Sessions(ids)) if ids == &[published]
    ));

    // A second drain with nothing published leaves the frame untouched.
    super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(backend.drain_events().is_empty());

    // Rows without stable identities are not actionable: the projection and
    // completion both fail closed instead of retaining the previous target.
    let (completions, events) = crate::usecase::application::daemon_backend::Completions::channel();
    pending_refresh = Some(completions);
    lane.queued
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push_back(Ok(SessionCommandResult {
            message: "legacy snapshot".to_owned(),
            sessions: Some(ui.workspace.sessions().to_vec()),
            session_ids: None,
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: Some(8),
        }));
    super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(ui.workspace.sessions().is_empty());
    assert!(ui.workspace.session_ids().is_empty());
    assert!(matches!(
        events.try_recv().unwrap(),
        AppEvent::Backend(BackendEvent::Sessions(ids)) if ids.is_empty()
    ));
}

#[test]
fn malformed_session_identity_refreshes_clear_rows_ids_and_agent_targets() {
    let workspace = WorkspaceId::new();
    let first = SessionId::new();
    let second = SessionId::new();
    let mut records = state("demo").sessions;
    records.push(SessionRecord {
        name: "second".to_owned(),
        root: "/tmp/demo/.usagi/sessions/second".into(),
        ..records[0].clone()
    });

    for invalid_ids in [None, Some(vec![first]), Some(vec![first, first])] {
        let view = WorkspaceView::with_runtime_ids(
            ws("demo"),
            WorkspaceState {
                sessions: records.clone(),
                ..WorkspaceState::default()
            },
            vec![first, second],
        );
        let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                vec![first, second],
                Box::new(UnavailableAgentCommandPort),
            );

        super::apply_session_projection(
            &mut ui,
            Some(records.clone()),
            invalid_ids,
            None,
            None,
            None,
        );

        assert!(ui.workspace.sessions().is_empty());
        assert!(ui.workspace.session_ids().is_empty());
        assert!(ui.agent.as_ref().unwrap().sessions.is_empty());
    }
}

/// A lane that fails reports it once through the parked completion and
/// leaves the adopted snapshot alone, and a snapshot older than one already
/// adopted is discarded whichever lane observed it.
#[test]
fn a_failed_or_stale_lane_observation_never_rewrites_the_adopted_snapshot() {
    let session = SessionId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    ui.last_session_revision = 9;
    let (completions, events) = crate::usecase::application::daemon_backend::Completions::channel();
    let mut pending_refresh = Some(completions);
    let stale = SessionId::new();
    let mut lane = FakeSessionRefreshPort {
        wakes: Arc::default(),
        takes: Arc::default(),
        queued: Arc::new(Mutex::new(VecDeque::from([
            Err("daemon unavailable\ninternal detail".to_owned()),
            Ok(SessionCommandResult {
                message: "stale".to_owned(),
                sessions: Some(Vec::new()),
                session_ids: Some(vec![stale]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(3),
            }),
            Err("later daemon failure".to_owned()),
        ]))),
    };

    super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(pending_refresh.is_none());
    assert!(matches!(
        events.try_recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(notice))
            if notice.message == "daemon unavailable"
    ));
    assert_eq!(ui.workspace.session_ids(), &[session]);

    super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert_eq!(ui.workspace.session_ids(), &[session]);
    assert_eq!(ui.last_session_revision, 9);

    // A lane error without a parked reducer completion is intentionally
    // consumed without synthesizing an event.
    super::drain_session_refresh(&mut ui, &mut lane, &mut pending_refresh);
    assert!(events.try_recv().is_err());
}

#[test]
fn direct_controller_entry_binds_workspace_config_settings() {
    let mut keys = vec![Key::Char(':')];
    keys.extend("config".chars().map(Key::Char));
    keys.extend([
        Key::Enter,
        Key::Quit,
        Key::CtrlQ,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut factory = FixedBackendFactory {
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
    let mut settings = WorkspaceBindingSettingsPort::default();

    assert_eq!(
        run_workspace_controller_with_backend_and_config(
            &mut term,
            snapshot("direct-config"),
            &mut factory,
            &mut settings,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(settings.selected, vec![PathBuf::from("/tmp/direct-config")]);
    assert!(term.frames.iter().any(|frame| {
        let frame = frame.join("\n");
        frame.contains("Config")
            && frame.contains("Agent")
            && !frame.contains("Scope:")
            && frame.contains("direct-config")
    }));
}

#[test]
fn direct_deck_entry_uses_the_shared_workspace_composition() {
    let snapshot = snapshot("direct-deck");
    let registry = vec![snapshot.workspace.clone()];
    let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, Key::Char('y')]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = FixedBackendFactory {
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
        run_workspace_deck_with_backend_and_config(
            &mut term,
            snapshot,
            &registry,
            &mut loader,
            &mut factory,
            &mut settings,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(settings.selected, vec![PathBuf::from("/tmp/direct-deck")]);
}

#[test]
fn controller_loop_opens_the_create_form_from_the_new_session_row() {
    // An empty workspace shows only root and `+ new session`, so one Down
    // reaches the create entry deterministically.
    let snapshot = snapshot_with_generated_runtime_ids(
        ws("empty"),
        WorkspaceState {
            sessions: Vec::new(),
            root_notes: Scratchpad::default(),
            updated_at: now(),
        },
    );
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: snapshot.workspace_id,
        session_id: None,
        worktree_id: WorktreeId::new(),
    };
    // Down → + new session, Enter opens the create form, type a name, Esc
    // closes it, then Ctrl-Q + y detaches.
    let keys = [
        Key::Down,
        Key::Enter,
        Key::Char('a'),
        Key::Char('p'),
        Key::Char('i'),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    let result = run_workspace_controller(
        &mut term,
        snapshot,
        Box::new(UnavailableSessionCommandPort),
        Box::new(SuccessfulAgentPort(terminal.clone())),
        launch_port(Box::new(SuccessfulAgentPort(terminal))),
        Box::new(UnavailableDecisionCommandPort),
        Box::new(UnavailableEnvironmentStore),
        Box::new(NoDesktopNotifications),
        Box::new(NoMetrics),
        Box::new(UnavailablePrSnapshotPort),
        Box::new(UnavailableBrowserOpener),
    );

    assert!(matches!(result, Ok(Exit::Quit)));
    // The inline `+ new session` row rendered the typed name, confirming the
    // create-entry seam works through the controller loop. It is inline in the
    // sidebar, not a centered modal, so the old "New session" modal title never
    // appears.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("api"))
    );
    assert!(
        term.frames
            .iter()
            .all(|frame| !frame.join("\n").contains("New session"))
    );
}

#[test]
fn controller_loop_dispatches_each_ctrl_a_representation_once_to_the_session_port() {
    struct SignallingSessionPort {
        calls: Arc<AtomicUsize>,
        create_call: std::sync::mpsc::Sender<String>,
    }

    impl SessionCommandPort for SignallingSessionPort {
        fn execute(
            &self,
            _: &Workspace,
            _: Option<&SessionRecord>,
            command: SessionCommand,
        ) -> Result<SessionCommandResult, String> {
            let SessionCommand::Create { name, .. } = command else {
                return Err("unexpected session command".to_owned());
            };
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.create_call
                .send(name)
                .map_err(|error| error.to_string())?;
            Ok(SessionCommandResult::message("daemon accepted"))
        }
    }

    // The composition adapter normalizes a modified Ctrl+A to LineStart,
    // preserves a raw control byte as U+0001, and carries Home as Home. All
    // three must enter the same controller form and lifecycle dispatch path.
    for create_key in [Key::LineStart, Key::Char('\u{1}'), Key::Home] {
        let snapshot = snapshot_with_generated_runtime_ids(
            ws("empty"),
            WorkspaceState {
                sessions: Vec::new(),
                root_notes: Scratchpad::default(),
                updated_at: now(),
            },
        );
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: snapshot.workspace_id,
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let (create_call, observed_create) = std::sync::mpsc::channel();
        let keys = [
            create_key.clone(),
            Key::Char('a'),
            Key::Char('p'),
            Key::Char('i'),
            Key::Enter,
            Key::CtrlQ,
            Key::Char('y'),
        ];
        let mut term = FakeTerminal::with_keys_waiting_for_create(&keys, observed_create);

        let result = run_workspace_controller(
            &mut term,
            snapshot,
            Box::new(SignallingSessionPort {
                calls: calls.clone(),
                create_call,
            }),
            Box::new(SuccessfulAgentPort(terminal.clone())),
            launch_port(Box::new(SuccessfulAgentPort(terminal))),
            Box::new(UnavailableDecisionCommandPort),
            Box::new(UnavailableEnvironmentStore),
            Box::new(NoDesktopNotifications),
            Box::new(NoMetrics),
            Box::new(UnavailablePrSnapshotPort),
            Box::new(UnavailableBrowserOpener),
        );

        assert!(matches!(result, Ok(Exit::Quit)), "{create_key:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{create_key:?}");
        assert_eq!(term.observed_creates, ["api"], "{create_key:?}");
    }
}

#[test]
fn drain_session_completions_refluxes_create_failure_with_its_token() {
    let snapshot = snapshot("demo");
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let token = PendingToken::from_raw(41);

    // A create worker returned a display-safe daemon rejection (e.g. a name the
    // daemon refuses). The legacy path used to drop this on the floor; it must
    // now reflux as a controller notice so the user sees the failure.
    let (backend_completions, backend_receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let result = Err("daemon refused the session".to_owned());
    let completion = super::SessionBackendCompletion::Create {
        token,
        before: Vec::new(),
        completions: backend_completions,
    };
    super::emit_session_command_result(&result, &completion);
    ui.active_session_command = Some(1);
    ui.session_completion_sender
        .send(super::SessionCommandCompletion {
            command_id: 1,
            result,
            completion,
        })
        .unwrap();

    super::drain_session_completions(&mut ui);
    assert!(matches!(
        backend_receiver.recv().unwrap(),
        AppEvent::OperationResult(result)
            if result.token == token
                && !result.succeeded
                && result.created.is_none()
                && result.notice.as_ref().is_some_and(|notice| notice.message == "daemon refused the session")
    ));
}

#[test]
fn session_commands_reject_the_second_request_as_busy() {
    let snapshot = snapshot("demo");
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let (first_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let (second_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();

    assert!(super::begin_session_command(
        &mut ui,
        SessionCommand::List,
        super::SessionBackendCompletion::Remove {
            session: SessionId::new(),
            before: Vec::new(),
            completions: first_completions,
        },
    ));
    assert!(!super::begin_session_command(
        &mut ui,
        SessionCommand::List,
        super::SessionBackendCompletion::Remove {
            session: SessionId::new(),
            before: Vec::new(),
            completions: second_completions,
        },
    ));
}

#[test]
fn stale_session_completion_does_not_replace_a_newer_snapshot() {
    let snapshot = snapshot("demo");
    let original = snapshot.session_ids[0];
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let (newer_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let (older_completions, _) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let newer = SessionId::new();
    let mut newer_record = ui.workspace.sessions()[0].clone();
    newer_record.name = "newer".to_owned();

    ui.active_session_command = Some(2);
    ui.session_completion_sender
        .send(super::SessionCommandCompletion {
            command_id: 2,
            result: Ok(SessionCommandResult {
                message: "newer".to_owned(),
                sessions: Some(vec![newer_record]),
                session_ids: Some(vec![newer]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(2),
            }),
            completion: super::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: vec![original],
                completions: newer_completions,
            },
        })
        .unwrap();
    super::drain_session_completions(&mut ui);

    ui.active_session_command = Some(1);
    ui.session_completion_sender
        .send(super::SessionCommandCompletion {
            command_id: 1,
            result: Ok(SessionCommandResult {
                message: "older".to_owned(),
                sessions: Some(ui.workspace.sessions().to_vec()),
                session_ids: Some(vec![original]),
                agent_resumes: None,
                session_lifecycles: None,
                session_roles: None,
                revision: Some(1),
            }),
            completion: super::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: vec![newer],
                completions: older_completions,
            },
        })
        .unwrap();

    super::drain_session_completions(&mut ui);
    assert_eq!(ui.workspace.session_ids(), &[newer]);
    assert_eq!(ui.workspace.sessions()[0].name, "newer");
}

#[test]
fn drain_session_completions_refluxes_create_success_with_created_identity() {
    let snapshot = snapshot("demo");
    let existing = snapshot.session_ids[0];
    let created = SessionId::new();
    let mut records = snapshot.state.sessions.clone();
    let mut new_record = records[0].clone();
    new_record.name = "created".to_owned();
    records.push(new_record);
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let token = PendingToken::from_raw(42);
    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let result = Ok(SessionCommandResult {
        message: "created".to_owned(),
        sessions: Some(records),
        session_ids: Some(vec![existing, created]),
        agent_resumes: None,
        session_lifecycles: None,
        session_roles: None,
        revision: None,
    });
    let completion = super::SessionBackendCompletion::Create {
        token,
        before: vec![existing],
        completions,
    };
    super::emit_session_command_result(&result, &completion);
    ui.active_session_command = Some(1);

    ui.session_completion_sender
        .send(super::SessionCommandCompletion {
            command_id: 1,
            result,
            completion,
        })
        .unwrap();
    super::drain_session_completions(&mut ui);

    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::OperationResult(result)
            if result.token == token && result.succeeded && result.created == Some(created)
    ));
}

#[test]
fn session_snapshot_completion_preserves_fallback_and_reports_failure_once() {
    let existing = SessionId::new();
    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = super::SessionBackendCompletion::Remove {
        session: SessionId::new(),
        before: vec![existing],
        completions,
    };
    super::emit_session_command_result(
        &Ok(SessionCommandResult::message("legacy snapshot")),
        &completion,
    );
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Sessions(sessions)) if sessions == [existing]
    ));
    assert!(receiver.try_recv().is_err());

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = super::SessionBackendCompletion::Remove {
        session: SessionId::new(),
        before: vec![existing],
        completions,
    };
    super::emit_session_command_result(&Err("daemon unavailable".to_owned()), &completion);
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(notice)) if notice.message == "daemon unavailable"
    ));
    assert!(receiver.try_recv().is_err());

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = super::SessionBackendCompletion::Sleep {
        before: vec![existing],
        completions,
    };
    super::emit_session_command_result(&Ok(SessionCommandResult::message("slept")), &completion);
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Sessions(sessions)) if sessions == [existing]
    ));

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let completion = super::SessionBackendCompletion::Sleep {
        before: vec![existing],
        completions,
    };
    super::emit_session_command_result(&Err("sleep refused".to_owned()), &completion);
    assert!(matches!(
        receiver.recv().unwrap(),
        AppEvent::Backend(BackendEvent::Notice(notice)) if notice.message == "sleep refused"
    ));
}

#[derive(Clone, Copy)]
enum ConcurrentSessionRequest {
    Create(u64),
    Remove,
    Sleep,
}

struct BlockingSessionPort {
    existing: SessionId,
    created: SessionId,
    calls: Arc<Mutex<Vec<SessionCommand>>>,
    started: std::sync::mpsc::Sender<()>,
    release: Mutex<Receiver<()>>,
    block_once: AtomicBool,
}

impl SessionCommandPort for BlockingSessionPort {
    fn execute(
        &self,
        _: &Workspace,
        _: Option<&SessionRecord>,
        command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        self.calls.lock().unwrap().push(command.clone());
        if self.block_once.swap(false, Ordering::SeqCst) {
            let _ = self.started.send(());
            let _ = self.release.lock().unwrap().recv();
        }
        let session_ids = match command {
            SessionCommand::Create { .. } => {
                vec![self.existing, self.created]
            }
            SessionCommand::Remove { .. } => Vec::new(),
            _ => vec![self.existing],
        };
        Ok(SessionCommandResult {
            message: "completed".to_owned(),
            sessions: None,
            session_ids: Some(session_ids),
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: None,
        })
    }
}

fn enqueue_session_request(
    host: &mut ControllerHost,
    request: ConcurrentSessionRequest,
    workspace: WorkspaceId,
    session: SessionId,
) -> Receiver<AppEvent> {
    use crate::usecase::application::daemon_backend::SessionCommandPort as _;

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    match request {
        ConcurrentSessionRequest::Create(token) => host.create(
            crate::usecase::application::daemon_backend::CreateSessionRequest {
                workspace,
                token: PendingToken::from_raw(token),
                operation_id: OperationId::new(),
                intent: SessionCreateIntent {
                    name: format!("session-{token}"),
                    base_ref: None,
                    profile: None,
                    model: None,
                    role_id: None,
                },
            },
            completions,
        ),
        ConcurrentSessionRequest::Remove => host.remove(
            crate::usecase::application::daemon_backend::RemoveSessionRequest {
                workspace,
                session,
                force: false,
                force_delete_branch: false,
                purge_orphan: false,
            },
            completions,
        ),
        ConcurrentSessionRequest::Sleep => host.sleep(
            crate::usecase::application::daemon_backend::SleepSessionRequest { workspace, session },
            completions,
        ),
    }
    receiver
}

fn assert_busy_pair(first: ConcurrentSessionRequest, second: ConcurrentSessionRequest) {
    let snapshot = snapshot("demo");
    let workspace = snapshot.workspace_id;
    let session = snapshot.session_ids[0];
    let created = SessionId::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(
        view,
        Box::new(BlockingSessionPort {
            existing: session,
            created,
            calls: calls.clone(),
            started: started_tx,
            release: Mutex::new(release_rx),
            block_once: AtomicBool::new(true),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (mut host, actions) = ControllerHost::channel();
    let first_completion = enqueue_session_request(&mut host, first, workspace, session);
    drain_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();

    let second_completion = enqueue_session_request(&mut host, second, workspace, session);
    drain_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    let busy = second_completion
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(match busy {
        AppEvent::OperationResult(result) => {
            !result.succeeded
                && result
                    .notice
                    .is_some_and(|notice| notice.message == "session command is already running")
        }
        AppEvent::Backend(BackendEvent::Notice(notice)) => {
            notice.message == "session command is already running"
        }
        _ => false,
    });
    assert!(second_completion.try_recv().is_err());

    release_tx.send(()).unwrap();
    first_completion
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(first_completion.try_recv().is_err());
    for _ in 0..100 {
        drain_session_completions(&mut ui);
        if ui.active_session_command.is_none() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(ui.active_session_command.is_none());
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[test]
fn concurrent_create_create_completes_second_as_busy() {
    assert_busy_pair(
        ConcurrentSessionRequest::Create(1),
        ConcurrentSessionRequest::Create(2),
    );
}

#[test]
fn concurrent_create_remove_completes_second_as_busy() {
    assert_busy_pair(
        ConcurrentSessionRequest::Create(1),
        ConcurrentSessionRequest::Remove,
    );
}

#[test]
fn concurrent_remove_create_completes_second_as_busy() {
    assert_busy_pair(
        ConcurrentSessionRequest::Remove,
        ConcurrentSessionRequest::Create(2),
    );
}

struct PanicOnceSessionPort {
    existing: SessionId,
    created: SessionId,
    panics: AtomicBool,
}

impl SessionCommandPort for PanicOnceSessionPort {
    fn execute(
        &self,
        _: &Workspace,
        _: Option<&SessionRecord>,
        _: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        assert!(
            !self.panics.swap(false, Ordering::SeqCst),
            "fake session worker panic"
        );
        Ok(SessionCommandResult {
            message: "recovered".to_owned(),
            sessions: None,
            session_ids: Some(vec![self.existing, self.created]),
            agent_resumes: None,
            session_lifecycles: None,
            session_roles: None,
            revision: None,
        })
    }
}

#[test]
fn session_worker_panic_completes_and_returns_the_port() {
    let snapshot = snapshot("demo");
    let workspace = snapshot.workspace_id;
    let session = snapshot.session_ids[0];
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(
        view,
        Box::new(PanicOnceSessionPort {
            existing: session,
            created: SessionId::new(),
            panics: AtomicBool::new(true),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (mut host, actions) = ControllerHost::channel();
    let failed = enqueue_session_request(
        &mut host,
        ConcurrentSessionRequest::Create(1),
        workspace,
        session,
    );
    drain_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(matches!(
        failed
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::OperationResult(result)
            if !result.succeeded
                && result.notice.as_ref().is_some_and(|notice| notice.message == "session command worker failed")
    ));
    for _ in 0..100 {
        drain_session_completions(&mut ui);
        if ui.active_session_command.is_none() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(ui.active_session_command.is_none());

    let recovered = enqueue_session_request(
        &mut host,
        ConcurrentSessionRequest::Create(2),
        workspace,
        session,
    );
    drain_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(matches!(
        recovered
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::OperationResult(result) if result.succeeded
    ));
}

#[test]
fn closed_session_host_channel_completes_each_effect_once() {
    use crate::usecase::application::daemon_backend::SessionCommandPort as _;

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let (mut host, actions) = ControllerHost::channel();
    drop(actions);

    for request in [
        ConcurrentSessionRequest::Create(1),
        ConcurrentSessionRequest::Remove,
        ConcurrentSessionRequest::Sleep,
    ] {
        let completion = enqueue_session_request(&mut host, request, workspace, session);
        assert!(matches!(
            completion
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            AppEvent::OperationResult(_) | AppEvent::Backend(BackendEvent::Notice(_))
        ));
        assert!(completion.try_recv().is_err());
    }

    let (completions, completion) =
        crate::usecase::application::daemon_backend::Completions::channel();
    host.refresh(workspace, completions);
    assert!(matches!(
        completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::Backend(BackendEvent::Notice(_))
    ));
    assert!(completion.try_recv().is_err());
}

#[test]
fn out_of_order_session_completion_cannot_release_the_active_port() {
    let snapshot = snapshot("demo");
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    ui.active_session_command = Some(2);
    let result = Ok(SessionCommandResult::message("done"));
    let (completions, _) = crate::usecase::application::daemon_backend::Completions::channel();

    ui.session_completion_sender
        .send(super::SessionCommandCompletion {
            command_id: 1,
            result: result.clone(),
            completion: super::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: Vec::new(),
                completions,
            },
        })
        .unwrap();
    drain_session_completions(&mut ui);
    assert_eq!(ui.active_session_command, Some(2));

    let (completions, _) = crate::usecase::application::daemon_backend::Completions::channel();
    ui.session_completion_sender
        .send(super::SessionCommandCompletion {
            command_id: 2,
            result,
            completion: super::SessionBackendCompletion::Remove {
                session: SessionId::new(),
                before: Vec::new(),
                completions,
            },
        })
        .unwrap();
    drain_session_completions(&mut ui);
    assert_eq!(ui.active_session_command, None);
}

#[test]
fn workspace_exit_does_not_drop_the_admitted_effect_completion() {
    let snapshot = snapshot("demo");
    let workspace = snapshot.workspace_id;
    let session = snapshot.session_ids[0];
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(
        view,
        Box::new(BlockingSessionPort {
            existing: session,
            created: SessionId::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
            started: started_tx,
            release: Mutex::new(release_rx),
            block_once: AtomicBool::new(true),
        }),
    );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (mut host, actions) = ControllerHost::channel();
    let completion = enqueue_session_request(
        &mut host,
        ConcurrentSessionRequest::Create(1),
        workspace,
        session,
    );
    drain_host_actions(
        &actions,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    started_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    drop(ui);
    drop(runtime);
    drop(actions);
    release_tx.send(()).unwrap();

    assert!(matches!(
        completion
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap(),
        AppEvent::OperationResult(_)
    ));
    assert!(completion.try_recv().is_err());
}

#[test]
fn session_snapshot_adapter_preserves_reconciliation_boundary_for_pointer_state() {
    use crate::presentation::workspace_runtime::WorkspaceRuntime;
    use crate::usecase::application::controller::{HomeMode, Route};

    let snapshot = snapshot("demo");
    let workspace_id = snapshot.workspace_id;
    let session = snapshot.session_ids[0];
    let records = snapshot.state.sessions.clone();
    let view =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, snapshot.session_ids);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace_id, vec![session]);
    let _ = runtime.apply_event(AppEvent::Resize {
        width: 100,
        height: 30,
    });
    let _ = runtime.apply_event(sidebar_pointer_event(
        5,
        2,
        std::time::Duration::from_millis(1_000),
    ));

    let (completions, receiver) =
        crate::usecase::application::daemon_backend::Completions::channel();
    let result = Ok(SessionCommandResult {
        message: "same snapshot".to_owned(),
        sessions: Some(records),
        session_ids: Some(vec![session]),
        agent_resumes: None,
        session_lifecycles: None,
        session_roles: None,
        revision: None,
    });
    let completion = super::SessionBackendCompletion::Remove {
        session: SessionId::new(),
        before: vec![session],
        completions,
    };
    super::emit_session_command_result(&result, &completion);
    ui.active_session_command = Some(1);
    ui.session_completion_sender
        .send(super::SessionCommandCompletion {
            command_id: 1,
            result,
            completion,
        })
        .unwrap();
    super::drain_session_completions(&mut ui);
    let _ = runtime.apply_event(receiver.recv().unwrap());
    assert_eq!(runtime.state().sessions(), &[session]);
    let _ = runtime.apply_event(sidebar_pointer_event(
        5,
        2,
        std::time::Duration::from_millis(1_100),
    ));

    let _ = workspace_id;
    assert_eq!(runtime.state().active(), Some(session));
    assert!(matches!(
        runtime.state().route(),
        Route::Home(HomeMode::Switch)
    ));
}

/// The attach payload a daemon at `geometry` returns after producing
/// `bytes`: the daemon is the grid authority, so it parses every byte and
/// hands back a semantic checkpoint instead of the raw tail.
fn attach_checkpoint(
    bytes: &[u8],
    geometry: Geometry,
) -> crate::usecase::application::terminal_session::TerminalAttachScreen {
    use usagi_core::usecase::vt_screen::VtScreen;

    let mut screen = VtScreen::new(usize::from(geometry.rows), usize::from(geometry.cols));
    screen.advance(bytes);
    crate::usecase::application::terminal_session::TerminalAttachScreen::Checkpoint(Box::new(
        screen.checkpoint(),
    ))
}

/// A streaming agent port whose PTY attaches live from `replay`, then reports
/// the configured safe error on poll. It records each detach so the auto-close
/// path can be asserted end to end.
struct ScriptedAgentPort {
    terminal: TerminalRef,
    subscription: u64,
    replay: Vec<u8>,
    poll_error: Option<TerminalError>,
    detaches: Arc<Mutex<Vec<u64>>>,
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=terminal_reconnect_fake_port_contract
impl AgentCommandPort for ScriptedAgentPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Ok(AgentPaneAdmission {
            terminal: self.terminal.clone(),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Ok(TerminalAttach {
            subscription: TerminalSubscription {
                id: self.subscription,
                epoch: 1,
            },
            revision: 1,
            output_offset: self.replay.len() as u64,
            next_input_seq: None,
            screen: attach_checkpoint(&self.replay, geometry),
            exited: false,
        })
    }

    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.poll_error.map_or(Ok(Vec::new()), Err)
    }

    fn input_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _subscription: TerminalSubscription,
        _input_seq: u64,
        _operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        if bytes == b"fail" {
            Err(TerminalError::Unavailable)
        } else {
            Ok(TerminalInputOutcome::Written)
        }
    }

    fn detach_terminal(&mut self, _terminal: &TerminalRef, subscription: TerminalSubscription) {
        self.detaches.lock().unwrap().push(subscription.id);
    }
}

struct WheelRecordingPort {
    terminal: TerminalRef,
    replay: Vec<u8>,
    inputs: Arc<Mutex<Vec<Vec<u8>>>>,
    input_error: bool,
}

struct ScrollingAgentPort {
    terminal: TerminalRef,
    replay: Vec<u8>,
    output: Option<Vec<u8>>,
}

impl AgentCommandPort for ScrollingAgentPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Ok(AgentPaneAdmission {
            terminal: self.terminal.clone(),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Ok(TerminalAttach {
            subscription: TerminalSubscription { id: 1, epoch: 1 },
            revision: 1,
            output_offset: self.replay.len() as u64,
            next_input_seq: None,
            screen: attach_checkpoint(&self.replay, geometry),
            exited: false,
        })
    }

    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        let Some(output) = self.output.take() else {
            return Ok(Vec::new());
        };
        Ok(vec![TerminalChunk {
            start_offset: after_offset,
            end_offset: after_offset
                .saturating_add(u64::try_from(output.len()).expect("test output fits in u64")),
            data: output,
        }])
    }
}

impl AgentCommandPort for WheelRecordingPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Ok(AgentPaneAdmission {
            terminal: self.terminal.clone(),
            continuation: None,
            supervisor_run_id: None,
        })
    }

    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Ok(TerminalAttach {
            subscription: TerminalSubscription { id: 1, epoch: 1 },
            revision: 1,
            output_offset: self.replay.len() as u64,
            next_input_seq: None,
            screen: attach_checkpoint(&self.replay, geometry),
            exited: false,
        })
    }

    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        Ok(Vec::new())
    }

    fn input_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _subscription: TerminalSubscription,
        _input_seq: u64,
        _operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        if self.input_error {
            return Err(TerminalError::Unavailable);
        }
        self.inputs.lock().unwrap().push(bytes.to_vec());
        Ok(TerminalInputOutcome::Written)
    }
}

fn live_terminal_ref(workspace: WorkspaceId, session: SessionId) -> TerminalRef {
    TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
    }
}

/// Build a `WorkspaceIoRuntime` + `WorkspaceRuntime` with `port` as the daemon
/// transport, driven into Closeup with a focused live tab attached to
/// `terminal`. Mirrors the shell's launch → complete → focus → attach path.
fn focused_live_pane(
    workspace: WorkspaceId,
    session: SessionId,
    terminal: TerminalRef,
    port: Box<dyn AgentCommandPort>,
) -> (WorkspaceIoRuntime, WorkspaceRuntime) {
    focused_live_pane_of_kind(workspace, session, terminal, PaneKind::Agent, port)
}

fn focused_live_pane_of_kind(
    workspace: WorkspaceId,
    session: SessionId,
    terminal: TerminalRef,
    kind: PaneKind,
    port: Box<dyn AgentCommandPort>,
) -> (WorkspaceIoRuntime, WorkspaceRuntime) {
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, vec![session], port);
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    // The first managed session is already selected; Enter activates it.
    let _ = runtime.handle_key(Key::Enter);
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Session(session), operation, kind);
    let _ = runtime.complete_pane(Target::Session(session), operation, terminal.clone());
    let _ = runtime.focus_terminal(Target::Session(session), terminal.clone());
    ui.start_terminal_session(terminal, terminal_geometry(20, 80));
    (ui, runtime)
}

#[test]
fn an_exited_terminal_auto_closes_its_pane_and_detaches_through_the_runtime() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let detaches = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 5,
            replay: b"live!".to_vec(),
            poll_error: Some(TerminalError::Exited),
            detaches: Arc::clone(&detaches),
        }),
    );
    assert!(runtime.state().has_live_pane());

    // The per-frame poll sweep observes the exit, drops the tab, and detaches
    // the client subscription — the #1011 behavior lost in the migration.
    close_exited_panes(&mut ui, &mut runtime);

    assert!(runtime.active_pane().tabs().is_empty());
    assert!(!runtime.state().has_live_pane());
    assert_eq!(*detaches.lock().unwrap(), vec![5]);
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "an Agent exit must refresh sidebar and Garden membership immediately"
    );
}

/// What the shell asked of the daemon for each pane, so a test can assert
/// that a detached background tab costs no attach and no resume.
#[derive(Default)]
struct BackgroundLaneLog {
    attaches: Vec<TerminalRef>,
    polls: Vec<TerminalRef>,
    watched: Vec<Vec<TerminalRef>>,
}

/// A port whose background lane is scripted: `exited` is what the bounded
/// per-scope inventory has observed, drained the way the production pump
/// hands its queue to the render thread.
struct BackgroundLanePort {
    log: Arc<Mutex<BackgroundLaneLog>>,
    exited: Arc<Mutex<Vec<TerminalRef>>>,
}

impl AgentCommandPort for BackgroundLanePort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("unused".to_owned())
    }

    fn attach_terminal(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        self.log.lock().unwrap().attaches.push(terminal.clone());
        Ok(TerminalAttach {
            subscription: TerminalSubscription { id: 1, epoch: 1 },
            revision: 1,
            output_offset: 0,
            next_input_seq: None,
            screen: attach_checkpoint(b"", geometry),
            exited: false,
        })
    }

    fn poll_terminal(
        &mut self,
        terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.log.lock().unwrap().polls.push(terminal.clone());
        Ok(Vec::new())
    }

    fn watch_background_terminals(&mut self, terminals: &[TerminalRef]) {
        self.log.lock().unwrap().watched.push(terminals.to_vec());
    }

    fn take_exited_background_terminals(&mut self, limit: usize) -> Vec<TerminalRef> {
        let mut exited = self.exited.lock().unwrap();
        let taken = exited.len().min(limit);
        exited.drain(..taken).collect()
    }
}

/// A focused foreground tab plus one background tab in the same target, the
/// shape #506 leaves behind: only the selection is attached.
fn foreground_and_background_panes(
    port: Box<dyn AgentCommandPort>,
) -> (
    WorkspaceIoRuntime,
    WorkspaceRuntime,
    TerminalRef,
    TerminalRef,
) {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let background = live_terminal_ref(workspace, session);
    let foreground = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(workspace, session, background.clone(), port);
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
    let _ = runtime.complete_pane(Target::Session(session), operation, foreground.clone());
    let _ = runtime.focus_terminal(Target::Session(session), foreground.clone());
    // The shell keeps exactly the selection attached; the first tab is now a
    // detached background tab.
    ui.sync_foreground_terminal(Some(&foreground), terminal_geometry(20, 80));
    (ui, runtime, foreground, background)
}

#[test]
fn a_background_tab_is_watched_by_scope_inventory_and_never_attached_or_resumed() {
    let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
    let exited = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime, foreground, background) =
        foreground_and_background_panes(Box::new(BackgroundLanePort {
            log: Arc::clone(&log),
            exited: Arc::clone(&exited),
        }));

    close_exited_panes(&mut ui, &mut runtime);

    let recorded = log.lock().unwrap();
    assert_eq!(
        recorded.watched.last().cloned(),
        Some(vec![background.clone()]),
        "only the detached background tab is observed by scope inventory"
    );
    assert!(
        !recorded.polls.iter().any(|polled| polled == &background),
        "a background tab is never resumed"
    );
    assert!(
        !recorded
            .attaches
            .iter()
            .skip(1)
            .any(|attached| attached == &background),
        "a background tab is never re-attached once it leaves the foreground"
    );
    assert_eq!(
        recorded.polls,
        vec![foreground.clone()],
        "only the foreground selection is resumed"
    );
    assert_eq!(
        runtime.active_pane().tabs().len(),
        2,
        "neither tab is closed while both runtimes are live"
    );
}

#[test]
fn a_background_exit_observed_by_scope_inventory_closes_that_tab_only() {
    let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
    let exited = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime, foreground, background) =
        foreground_and_background_panes(Box::new(BackgroundLanePort {
            log: Arc::clone(&log),
            exited: Arc::clone(&exited),
        }));
    // The bounded inventory lane observed the background shell exiting.
    exited.lock().unwrap().push(background.clone());

    close_exited_panes(&mut ui, &mut runtime);

    let tabs = runtime.active_pane().tabs().to_vec();
    assert_eq!(tabs.len(), 1, "only the exited background tab is closed");
    assert!(
        matches!(&tabs[0], PaneTab::Live(live) if live.terminal.fences(&foreground)),
        "the foreground selection keeps streaming"
    );
    assert!(runtime.state().has_live_pane());
    assert!(
        ui.take_agent_inventory_change_observation_request(),
        "a background Agent exit must wake the coherent inventory lane"
    );
    // The closed tab stops being watched on the next frame.
    close_exited_panes(&mut ui, &mut runtime);
    assert_eq!(
        log.lock().unwrap().watched.last().cloned(),
        Some(Vec::new())
    );
}

#[test]
fn background_exits_are_applied_at_a_bounded_rate_per_frame() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let log = Arc::new(Mutex::new(BackgroundLaneLog::default()));
    let exited = Arc::new(Mutex::new(Vec::new()));
    let first = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        first.clone(),
        Box::new(BackgroundLanePort {
            log: Arc::clone(&log),
            exited: Arc::clone(&exited),
        }),
    );
    let mut background = vec![first];
    for _ in 0..MAX_BACKGROUND_EXITS_PER_FRAME + 3 {
        let terminal = live_terminal_ref(workspace, session);
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
        let _ = runtime.complete_pane(Target::Session(session), operation, terminal.clone());
        background.push(terminal);
    }
    let foreground = background.pop().expect("the last tab stays selected");
    let _ = runtime.focus_terminal(Target::Session(session), foreground.clone());
    ui.sync_foreground_terminal(Some(&foreground), terminal_geometry(20, 80));
    exited.lock().unwrap().extend(background.iter().cloned());

    close_exited_panes(&mut ui, &mut runtime);
    assert_eq!(
        runtime.active_pane().tabs().len(),
        background.len() + 1 - MAX_BACKGROUND_EXITS_PER_FRAME,
        "one frame applies at most the bounded slice of background exits"
    );
    // The remainder lands on the following frames, none of it lost.
    close_exited_panes(&mut ui, &mut runtime);
    close_exited_panes(&mut ui, &mut runtime);
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert!(runtime.state().has_live_pane());
}

/// The wire traffic one shared connection recorded, as `e<epoch> <op> <label>`.
type SharedConnectionLog = Arc<Mutex<Vec<String>>>;
/// The bytes each labelled pane wrote to its PTY, in order.
type SharedConnectionWrites = Arc<Mutex<Vec<(&'static str, Vec<u8>)>>>;

/// Failures armed between steps of the shared-connection scenario, so each
/// one happens at exactly the point the test drives it.
#[derive(Default)]
struct SharedConnectionScript {
    /// Terminals whose next poll answers `resync_required`.
    poll_resync: Vec<&'static str>,
    /// Terminals whose next viewport resize fails on the resize lane.
    resize_failures: Vec<&'static str>,
    /// Terminals whose next input loses the transport mid-response.
    input_transport_eof: Vec<&'static str>,
}

/// One shared daemon connection carrying every pane's attach / input /
/// detach, as the production adapter does.
///
/// Replacing that connection releases **all** of its attachments and starts
/// a fresh per-connection input ledger — the daemon's own behavior — so a
/// subscription taken before the replacement is no longer usable by anyone.
/// Every request is recorded as `e<epoch> <op> <label>` so each pane's
/// ordering within an epoch can be asserted.
struct SharedConnectionPort {
    labels: Vec<(TerminalRef, &'static str)>,
    epoch: u64,
    next_subscription: u64,
    /// The subscriptions the live connection holds, as the daemon sees them.
    attached: Vec<(TerminalRef, u64)>,
    /// The next input sequence the daemon expects on this connection.
    ledger: Vec<(TerminalRef, u64)>,
    /// Durable input operations the daemon recorded. Unlike `ledger` this
    /// survives the connection, which is what lets a client resolve an
    /// acknowledgement it lost (#519).
    recorded_operations: Vec<OperationId>,
    script: Arc<Mutex<SharedConnectionScript>>,
    log: SharedConnectionLog,
    writes: SharedConnectionWrites,
}

impl SharedConnectionPort {
    fn label(&self, terminal: &TerminalRef) -> &'static str {
        self.labels
            .iter()
            .find(|(candidate, _)| candidate.fences(terminal))
            .map(|(_, label)| *label)
            .expect("every terminal in this scenario is labelled")
    }

    fn record(&self, event: String) {
        self.log.lock().unwrap().push(event);
    }

    /// Consumes one armed failure for `label`.
    fn take_armed(
        &self,
        label: &'static str,
        select: fn(&mut SharedConnectionScript) -> &mut Vec<&'static str>,
    ) -> bool {
        let mut script = self.script.lock().unwrap();
        let list = select(&mut script);
        match list.iter().position(|entry| *entry == label) {
            Some(index) => {
                list.remove(index);
                true
            }
            None => false,
        }
    }

    /// The transport broke mid-request: the daemon drops every attachment of
    /// that connection, and the client's next request runs on a new one.
    fn replace_transport(&mut self) {
        self.epoch += 1;
        self.attached.clear();
        self.ledger.clear();
        self.record(format!("e{} replaced", self.epoch));
    }

    fn holds(&self, terminal: &TerminalRef, subscription: u64) -> bool {
        self.attached
            .iter()
            .any(|(attached, id)| attached.fences(terminal) && *id == subscription)
    }

    fn expected_seq(&self, terminal: &TerminalRef) -> u64 {
        self.ledger
            .iter()
            .find(|(attached, _)| attached.fences(terminal))
            .map_or(0, |(_, seq)| *seq)
    }
}

impl AgentCommandPort for SharedConnectionPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("this scenario attaches already-launched terminals".to_owned())
    }

    fn terminal_connection_epoch(&self) -> Option<u64> {
        Some(self.epoch)
    }

    fn resize_terminal(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        let label = self.label(terminal);
        // `Resize` rides its own deadline-bounded lane, so even its transport
        // failure leaves the shared connection — and every attachment on it —
        // alone.
        if self.take_armed(label, |script| &mut script.resize_failures) {
            self.record(format!("e{} resize-failed {label}", self.epoch));
            return Err(TerminalError::Unavailable);
        }
        self.record(format!("e{} resize {label}", self.epoch));
        Ok(geometry)
    }

    fn attach_terminal(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        let label = self.label(terminal);
        self.next_subscription += 1;
        let id = self.next_subscription;
        self.attached.push((terminal.clone(), id));
        self.record(format!("e{} attach {label}", self.epoch));
        Ok(TerminalAttach {
            subscription: TerminalSubscription {
                id,
                epoch: self.epoch,
            },
            revision: 1,
            output_offset: 0,
            next_input_seq: None,
            screen: attach_checkpoint(b"", geometry),
            exited: false,
        })
    }

    fn poll_terminal(
        &mut self,
        terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        let label = self.label(terminal);
        // A fully received `resync_required` is a finished answer: it tells
        // one pane to replace its screen, not the whole TUI to reconnect.
        if self.take_armed(label, |script| &mut script.poll_resync) {
            self.record(format!("e{} resync-required {label}", self.epoch));
            return Err(TerminalError::ResyncRequired);
        }
        self.record(format!("e{} resume {label}", self.epoch));
        Ok(Vec::new())
    }

    fn input_terminal(
        &mut self,
        terminal: &TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        let label = self.label(terminal);
        // What the daemon does with a subscription whose connection is gone:
        // it released that attachment, so the write is refused with no effect
        // and the keystroke is lost. No pane may ever reach this.
        if subscription.epoch != self.epoch || !self.holds(terminal, subscription.id) {
            self.record(format!("e{} not-attached {label}", self.epoch));
            return Err(TerminalError::Stale);
        }
        let expected = self.expected_seq(terminal);
        if input_seq != expected {
            self.record(format!(
                "e{} sequence-gap {label} (got {input_seq}, want {expected})",
                self.epoch
            ));
            return Err(TerminalError::Stale);
        }
        if self.take_armed(label, |script| &mut script.input_transport_eof) {
            // The daemon applied the write and recorded its operation; only
            // the response was lost. That is the case #519 has to converge:
            // the client must resolve the operation, not resend the bytes.
            self.apply(terminal, label, input_seq, bytes);
            self.recorded_operations.push(operation);
            self.replace_transport();
            return Err(TerminalError::InputEffectUnknown);
        }
        self.apply(terminal, label, input_seq, bytes);
        self.recorded_operations.push(operation);
        Ok(TerminalInputOutcome::Written)
    }

    fn terminal_input_outcome(
        &mut self,
        terminal: &TerminalRef,
        operation: OperationId,
        _input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        let label = self.label(terminal);
        self.record(format!("e{} input-outcome {label}", self.epoch));
        Ok(if self.recorded_operations.contains(&operation) {
            TerminalInputResolution::Final(TerminalInputOutcome::Written)
        } else {
            TerminalInputResolution::Unknown
        })
    }

    fn detach_terminal(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription) {
        let label = self.label(terminal);
        if subscription.epoch != self.epoch {
            // Released locally: the daemon already dropped this attachment
            // with its connection, so nothing on the current one is touched.
            self.record(format!("e{} local-detach {label}", self.epoch));
            return;
        }
        self.attached
            .retain(|(attached, id)| !(attached.fences(terminal) && *id == subscription.id));
        self.record(format!("e{} detach {label}", self.epoch));
    }
}

impl SharedConnectionPort {
    /// Records one accepted write exactly as the daemon would.
    fn apply(&mut self, terminal: &TerminalRef, label: &'static str, input_seq: u64, bytes: &[u8]) {
        match self
            .ledger
            .iter_mut()
            .find(|(attached, _)| attached.fences(terminal))
        {
            Some((_, seq)) => *seq += 1,
            None => self.ledger.push((terminal.clone(), 1)),
        }
        self.writes.lock().unwrap().push((label, bytes.to_vec()));
        self.record(format!("e{} input#{input_seq} {label}", self.epoch));
    }
}

/// Every attachment-fenced request one pane made in one epoch, in order.
fn fenced_traffic(log: &[String], epoch: &str, label: &str) -> Vec<String> {
    log.iter()
        .filter(|event| {
            event.starts_with(epoch)
                && event.ends_with(label)
                && (event.contains(" attach ")
                    || event.contains(" resume ")
                    || event.contains(" input#"))
        })
        .cloned()
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)] // One scenario drives every epoch transition in order.
fn a_replaced_shared_connection_reattaches_every_pane_before_it_streams_again() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let agent = live_terminal_ref(workspace, session);
    let generic = live_terminal_ref(workspace, session);
    let script = Arc::new(Mutex::new(SharedConnectionScript::default()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(SharedConnectionPort {
                labels: vec![(agent.clone(), "A"), (generic.clone(), "B")],
                epoch: 1,
                next_subscription: 10,
                attached: Vec::new(),
                ledger: Vec::new(),
                recorded_operations: Vec::new(),
                script: Arc::clone(&script),
                log: Arc::clone(&log),
                writes: Arc::clone(&writes),
            }),
        );
    let geometry = terminal_geometry(20, 80);

    // Both panes attach over one connection and type once.
    ui.start_terminal_session(agent.clone(), geometry);
    ui.start_terminal_session(generic.clone(), geometry);
    assert_eq!(ui.send_terminal_bytes(&agent, b"a"), Ok(()));
    assert_eq!(ui.send_terminal_bytes(&generic, b"b"), Ok(()));

    // 1. Pane A's poll takes a fully received `resync_required`. It resyncs on
    //    the same connection, so B keeps its attachment and its ledger
    //    position, and A continues from the sequence the daemon expects.
    script.lock().unwrap().poll_resync.push("A");
    assert!(ui.poll_all_terminals().is_empty());
    assert_eq!(ui.send_terminal_bytes(&generic, b"b2"), Ok(()));
    assert_eq!(ui.send_terminal_bytes(&agent, b"a2"), Ok(()));

    // 2. A's viewport resize fails on the resize lane. Neither pane loses its
    //    attachment, so both keep writing on the same subscriptions.
    script.lock().unwrap().resize_failures.push("A");
    ui.resize_terminals(terminal_geometry(24, 100));
    assert_eq!(ui.send_terminal_bytes(&agent, b"a3"), Ok(()));

    // 3. A's input loses the transport before its response completes. The
    //    daemon released B's attachment with that connection too, even though
    //    B never saw a failure.
    script.lock().unwrap().input_transport_eof.push("A");
    assert!(ui.send_terminal_bytes(&agent, b"a4").is_err());

    // B's very next keystroke attaches on the new connection first, and is
    // written exactly once instead of being rejected as unattached.
    assert_eq!(ui.send_terminal_bytes(&generic, b"k"), Ok(()));

    // A recovers through its own reconnect backoff, then both panes stream.
    for _ in 0..200 {
        if ui.poll_all_terminals().is_empty()
            && fenced_traffic(&log.lock().unwrap(), "e2", "A")
                .iter()
                .any(|event| event.contains(" attach "))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // A's lost acknowledgement fenced its producer queue, so reattaching is
    // not enough: the next tick resolves that operation against the daemon's
    // durable record before any later keystroke may reach the PTY (#519).
    assert_eq!(
        ui.send_terminal_bytes(&agent, b"a4-next"),
        Err("terminal input is held in order behind an unresolved input (1 waiting)".to_owned())
    );
    ui.poll_all_terminals();
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|event| event == "e2 input-outcome A")
    );
    // The held keystroke was delivered in order once the fence converged.
    assert_eq!(
        writes.lock().unwrap().last(),
        Some(&("A", b"a4-next".to_vec()))
    );
    assert_eq!(ui.send_terminal_bytes(&agent, b"a5"), Ok(()));

    // Releasing A's pane at the end must not disturb B's attachment.
    ui.close_terminal(&agent);
    assert_eq!(ui.send_terminal_bytes(&generic, b"k2"), Ok(()));
    // Returning to A on the same connection revives its retained coordinator
    // and continues at the daemon ledger cursor instead of restarting at 0.
    ui.start_terminal_session(agent.clone(), geometry);
    assert_eq!(ui.send_terminal_bytes(&agent, b"a6"), Ok(()));

    let log = log.lock().unwrap().clone();
    // No keystroke was ever spent on a released subscription, and no ledger
    // gap opened: the exact cascade this fences off.
    assert!(
        !log.iter()
            .any(|event| event.contains("not-attached") || event.contains("sequence-gap")),
        "{log:#?}"
    );
    // In each epoch, every pane's first attachment-fenced request is its own
    // attach — never a `Resume` or an `Input` on a released subscription.
    for epoch in ["e1", "e2"] {
        for label in ["A", "B"] {
            let traffic = fenced_traffic(&log, epoch, label);
            assert_eq!(
                traffic.first(),
                Some(&format!("{epoch} attach {label}")),
                "{label} in {epoch}: {log:#?}"
            );
        }
    }
    // Exactly one connection replacement happened, and only the failing lane
    // caused it.
    assert_eq!(
        log.iter()
            .filter(|event| event.contains("replaced"))
            .count(),
        1,
        "{log:#?}"
    );
    // B held a subscription from the replaced connection, so its release was
    // local: it was never re-sent on the connection its peers now use, and it
    // came after — and did not revoke — the attach that replaced it.
    assert!(log.contains(&"e2 local-detach B".to_owned()), "{log:#?}");
    // A's same-connection resync detached its own superseded subscription
    // there, where the daemon still held it, and closing A's pane later
    // released its current one the same way.
    assert!(log.contains(&"e1 detach A".to_owned()), "{log:#?}");
    assert!(log.contains(&"e2 detach A".to_owned()), "{log:#?}");
    // Sequences continue across a same-connection resync and restart only on
    // the new connection's fresh ledger.
    for (label, expected) in [
        (
            'A',
            vec![
                "e1 input#0 A",
                "e1 input#1 A",
                "e1 input#2 A",
                // The write whose acknowledgement was lost: the daemon
                // applied it once, which is exactly what the client cannot
                // know until it resolves the operation.
                "e1 input#3 A",
                // The held keystroke, then the one typed after the fence
                // converged. Both on the fresh epoch's restarted sequence.
                "e2 input#0 A",
                "e2 input#1 A",
                // Detach/re-attach on e2 retains the coordinator ledger.
                "e2 input#2 A",
            ],
        ),
        (
            'B',
            vec![
                "e1 input#0 B",
                "e1 input#1 B",
                "e2 input#0 B",
                "e2 input#1 B",
            ],
        ),
    ] {
        assert_eq!(
            log.iter()
                .filter(|event| event.contains(" input#") && event.ends_with(label))
                .cloned()
                .collect::<Vec<_>>(),
            expected,
            "{log:#?}"
        );
    }

    // Every keystroke reached the PTY once, in order, including the first one
    // after the recovery.
    assert_eq!(
        writes.lock().unwrap().clone(),
        vec![
            ("A", b"a".to_vec()),
            ("B", b"b".to_vec()),
            ("B", b"b2".to_vec()),
            ("A", b"a2".to_vec()),
            ("A", b"a3".to_vec()),
            // Applied before the response was lost, and never applied twice.
            ("A", b"a4".to_vec()),
            ("B", b"k".to_vec()),
            // Released from the fence in production order.
            ("A", b"a4-next".to_vec()),
            ("A", b"a5".to_vec()),
            ("B", b"k2".to_vec()),
            ("A", b"a6".to_vec()),
        ]
    );
}

#[test]
fn reconnecting_and_stale_terminal_states_are_projected_into_the_pane_footer() {
    for (error, expected) in [
        (
            TerminalError::Unavailable,
            "daemon unavailable; reconnecting",
        ),
        (TerminalError::Stale, "terminal is no longer available"),
    ] {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (mut ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 6,
                replay: b"retained".to_vec(),
                poll_error: Some(error),
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        );
        let mut controls = LiveTerminalControls::default();

        assert!(ui.poll_all_terminals().is_empty());
        let view = controller_terminal_view(&ui, &runtime, &mut controls, 10).unwrap();

        assert_eq!(view.feedback.as_deref(), Some(expected));
        assert_eq!(view.rows[0], "retained");
    }
}

#[test]
fn terminal_reconnect_fake_port_contract() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, _runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 17,
            replay: Vec::new(),
            poll_error: Some(TerminalError::Unavailable),
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );

    assert!(ui.poll_all_terminals().is_empty());
    assert!(!ui.take_terminal_reconnected());
    std::thread::sleep(std::time::Duration::from_millis(110));
    assert!(ui.poll_all_terminals().is_empty());
    assert!(ui.take_terminal_reconnected());
    assert!(!ui.take_terminal_reconnected());
}

#[test]
fn close_tab_live_action_sends_ctrl_d_to_the_focused_agent() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let (ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(WheelRecordingPort {
            terminal,
            replay: Vec::new(),
            inputs: Arc::clone(&inputs),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut ui = ui;
    let mut pending_targets = std::collections::HashMap::new();

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
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

    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert!(ui.closed_generic_terminals.is_empty());
    assert_eq!(*inputs.lock().unwrap(), vec![vec![4]]);
    assert!(runtime.state().notice().is_none());
}

#[test]
fn generic_terminal_ctrl_l_and_ctrl_c_clear_the_shell_and_close_requests_exit() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane_of_kind(
        workspace,
        session,
        terminal.clone(),
        PaneKind::Terminal,
        Box::new(WheelRecordingPort {
            terminal: terminal.clone(),
            replay: b"one\r\ntwo\r\nthree".to_vec(),
            inputs: Arc::clone(&inputs),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Passthrough(vec![12]),
    ));
    assert!(
        !ui.terminal_rows(&terminal, None)
            .expect("focused terminal remains attached")
            .join("\n")
            .contains("one")
    );
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Quit,
    ));

    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
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

    assert_eq!(
        *inputs.lock().unwrap(),
        vec![
            b"\x0c".to_vec(),
            b"\x03\x0c".to_vec(),
            b"\x03exit\r".to_vec()
        ]
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.contains(&terminal));
}

#[test]
fn close_tab_live_action_surfaces_a_safe_delivery_failure() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane_of_kind(
        workspace,
        session,
        terminal.clone(),
        PaneKind::Terminal,
        Box::new(WheelRecordingPort {
            terminal,
            replay: b"retained".to_vec(),
            inputs: Arc::clone(&inputs),
            input_error: true,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
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

    assert!(inputs.lock().unwrap().is_empty());
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(
        runtime.active_pane().error(),
        Some("daemon unavailable; keystroke not delivered")
    );
    assert!(runtime.state().notice().is_none());
}

#[test]
fn focused_pane_feedback_is_visible_in_a_live_terminal_footer() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(WheelRecordingPort {
            terminal,
            replay: b"retained".to_vec(),
            inputs: Arc::new(Mutex::new(Vec::new())),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();

    runtime.surface_focused_pane_feedback("Agent close input was not delivered");
    let view = controller_terminal_view(&ui, &runtime, &mut controls, 10).unwrap();

    assert_eq!(
        view.feedback.as_deref(),
        Some("Agent close input was not delivered")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers every wheel route with shared pane geometry.
fn physical_wheel_follows_full_screen_program_input_modes() {
    let cases = [
        (
            b"\x1b[?1000h\x1b[?1006hclaude".as_slice(),
            Some(b"\x1b[<64;5;1M".repeat(3)),
        ),
        (
            b"\x1b[?1049h\x1b[?1hcodex".as_slice(),
            Some(b"\x1bOA".repeat(super::WHEEL_LINES * 3)),
        ),
        (b"\x1b[?1000hclaude".as_slice(), None),
    ];

    for (replay, expected) in cases {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(WheelRecordingPort {
                terminal,
                replay: replay.to_vec(),
                inputs: Arc::clone(&inputs),
                input_error: expected.is_none(),
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let geometry = terminal_geometry(20, 80);
        let (_, rows_len, scroll) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        let mut term = FakeTerminal::default();
        let mut browser = RecordingBrowser::default();
        let mut pending = std::collections::HashMap::new();

        assert!(intercept_live_terminal_control(
            &Key::Live(LiveTerminalAction::Wheel {
                up: true,
                column: 41,
                row: 5,
                notches: 3,
            }),
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
        assert_eq!(inputs.lock().unwrap().as_slice(), expected.as_slice());
        if expected.is_none() {
            assert!(controls.project(Vec::new(), 1).feedback.is_some());
        }
    }

    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let mut replay = String::new();
    for row in 0..30 {
        use std::fmt::Write as _;
        let _ = writeln!(replay, "row {row}\r");
    }
    let replay = replay.into_bytes();
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(WheelRecordingPort {
            terminal: terminal.clone(),
            replay,
            inputs: Arc::clone(&inputs),
            input_error: false,
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let geometry = terminal_geometry(20, 80);
    let (_, rows_len, scroll) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 0,
            row: 0,
            notches: 1,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        rows_len,
        scroll,
    ));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 41,
            row: 5,
            notches: 1,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        rows_len,
        scroll,
    ));
    let (view, _, _) = poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    assert_eq!(view.expect("primary history").scroll, super::WHEEL_LINES);
    assert!(inputs.lock().unwrap().is_empty());

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: false,
            column: 41,
            row: 5,
            notches: 1,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        rows_len,
        scroll,
    ));
    let (view, _, _) = poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    assert_eq!(view.expect("primary history").scroll, 0);

    ui.close_terminal(&terminal);
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 41,
            row: 5,
            notches: 1,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        rows_len,
        scroll,
    ));

    let empty_view = WorkspaceView::with_runtime_ids(ws("empty"), empty_state("empty"), vec![]);
    let mut empty_ui = WorkspaceIoRuntime::new(empty_view, Box::new(UnavailableSessionCommandPort));
    let mut empty_runtime = WorkspaceRuntime::new(WorkspaceId::new(), vec![]);
    let _ = empty_runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let drawer = crate::presentation::director_drawer::geometry(20, 80);
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: u16::try_from(drawer.left.saturating_add(2)).expect("drawer column"),
            row: u16::try_from(drawer.top.saturating_add(4)).expect("drawer row"),
            notches: 1,
        }),
        &mut empty_ui,
        &mut empty_runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        rows_len,
        scroll,
    ));
}

#[test]
fn paste_markers_follow_the_focused_programs_bracketed_paste_mode() {
    for (replay, expected) in [
        (b"agent".as_slice(), b"one\ntwo".to_vec()),
        (
            b"\x1b[?2004hagent".as_slice(),
            b"\x1b[200~one\ntwo\x1b[201~".to_vec(),
        ),
        (
            b"\x1b[?2004h\x1b[?2004lagent".as_slice(),
            b"one\ntwo".to_vec(),
        ),
    ] {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let (mut ui, runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(WheelRecordingPort {
                terminal,
                replay: replay.to_vec(),
                inputs: Arc::clone(&inputs),
                input_error: false,
            }),
        );
        let mut controls = LiveTerminalControls::default();
        let mut term = FakeTerminal::default();

        assert!(forward_live_terminal_input(
            &mut ui,
            &runtime,
            &mut controls,
            &mut term,
            &Key::Paste("one\ntwo".to_owned()),
        ));
        assert_eq!(*inputs.lock().unwrap(), vec![expected]);
    }
}

/// `Ctrl-O End` is the way back to live output. A scrolled viewport holds its
/// rows against everything the Agent appends, so the distance to the newest
/// output grows with the conversation and one-line `ScrollDown` alone cannot
/// be the only way back.
#[test]
fn scroll_bottom_returns_a_scrolled_pane_to_the_live_output() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let mut replay = String::new();
    for row in 0..40 {
        use std::fmt::Write as _;
        let _ = writeln!(replay, "row {row}\r");
    }
    let replay = replay.into_bytes();
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 11,
            replay,
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let geometry = terminal_geometry(20, 80);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let mut scroll_key = |key,
                          ui: &mut WorkspaceIoRuntime,
                          runtime: &mut WorkspaceRuntime,
                          controls: &mut LiveTerminalControls,
                          rows_len,
                          scroll| {
        assert!(intercept_live_terminal_control(
            &Key::Live(key),
            ui,
            runtime,
            controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            rows_len,
            scroll,
        ));
    };

    let (view, rows_len, scroll) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let live_bottom = view.expect("the focused live tab projects its rows");
    assert_eq!(live_bottom.scroll, 0);

    for _ in 0..5 {
        scroll_key(
            LiveTerminalAction::ScrollUp,
            &mut ui,
            &mut runtime,
            &mut controls,
            rows_len,
            scroll,
        );
    }
    let (scrolled, rows_len, scroll) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let scrolled = scrolled.expect("a scrolled viewport still projects rows");
    assert_eq!(scrolled.scroll, 5);
    assert_ne!(scrolled.rows, live_bottom.rows);

    scroll_key(
        LiveTerminalAction::ScrollBottom,
        &mut ui,
        &mut runtime,
        &mut controls,
        rows_len,
        scroll,
    );
    let (followed, _, _) =
        poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    assert_eq!(
        followed.expect("the pane follows live output again"),
        live_bottom
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture audits every pane-only and reducer-owned key.
fn switch_consumes_right_pane_controls_without_mutating_the_dimmed_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let detaches = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 18,
            replay: b"one\ntwo\nthree\nhttps://example.com".to_vec(),
            poll_error: None,
            detaches: Arc::clone(&detaches),
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let rows = vec![
        "one".to_owned(),
        "two".to_owned(),
        "three".to_owned(),
        "https://example.com".to_owned(),
    ];
    let _ = controls.project(rows.clone(), 1);
    controls.scroll_up();
    let before = controls.project(rows.clone(), 1).scroll;
    assert_eq!(before, 1);
    let tabs_before = runtime.active_pane().tabs().to_vec();

    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::OpenCloseupModal));
    assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::ScrollUp),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        4,
        before,
    ));
    let _ = runtime.handle_key(Key::Escape);
    assert!(runtime.wants_live_input());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Switch));
    assert!(!runtime.wants_live_input());
    for key in [
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
        Key::Pointer(PointerEvent {
            kind: PointerKind::Up,
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
            4,
            before,
        ));
    }

    for key in [
        Key::Live(LiveTerminalAction::NextTab),
        Key::Passthrough(Vec::new()),
        Key::TerminalCopy {
            fallback: Vec::new(),
        },
        Key::Up,
        Key::Down,
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
        Key::Enter,
        Key::Backspace,
        Key::Tab,
        Key::Escape,
        Key::Quit,
        Key::CtrlQ,
        Key::CtrlD,
        Key::CtrlX,
        Key::Char('x'),
        Key::Click { column: 41, row: 5 },
        Key::Other,
    ] {
        assert!(!intercept_live_terminal_control(
            &key,
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            4,
            before,
        ));
    }
    assert_eq!(controls.project(rows, 1).scroll, before);
    assert!(!controls.has_selection());
    assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
    assert!(detaches.lock().unwrap().is_empty());
    assert!(term.copied.is_empty());
    assert!(browser.opened.is_empty());
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
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(runtime.state().director_drawer_open());
    assert_eq!(runtime.state().director_route(), DirectorRoute::WorkRuns);
    let tabs_before = runtime.active_pane().tabs().to_vec();
    let mut controls = LiveTerminalControls::default();
    let rows = vec!["one".to_owned(), "two".to_owned(), "three".to_owned()];
    let _ = controls.project(rows.clone(), 1);
    controls.scroll_up();
    let scroll_before = controls.project(rows.clone(), 1).scroll;
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let drawer = super::director_drawer::geometry(20, 80);
    let new_click = Key::Click {
        column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
        row: u16::try_from(drawer.top + 2).unwrap(),
    };
    assert!(super::is_director_new_click(&new_click, &runtime, 20, 80));
    let new_pointer = Key::Pointer(PointerEvent {
        kind: PointerKind::Down,
        column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
        row: u16::try_from(drawer.top + 2).unwrap(),
    });
    assert!(super::is_director_new_click(&new_pointer, &runtime, 20, 80));
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
    let launch_effects = super::open_director_from_new_button(
        &mut runtime,
        &new_pointer,
        20,
        80,
        super::WorkRunControlMode::Closed,
    )
    .expect("the Work Runs Start button owns its pointer press");
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
    assert!(!super::is_director_new_click(
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
            super::home_header_action_at(100, &home, *column, 0)
                == Some(super::HomeHeaderAction::RootTerminal)
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
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
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
    assert!(super::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    let _ = runtime.handle_key(Key::Escape);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: vec![LivePane {
                terminal: live.clone(),
                kind: PaneKind::Agent,
            }],
            selected: Some(live.clone()),
            selected_interrupted: None,
            interrupted: vec![interrupted.clone()],
        }],
    ));

    // Closed drawers deliberately project nothing.
    assert_eq!(
        super::director_drawer_projection(&ui, &runtime, None),
        super::DirectorDrawerProjection::default()
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let terminal_view = TerminalViewProjection {
        rows: vec!["retained output".to_owned()],
        row_offset: 0,
        total_rows: 1,
        scroll: 0,
        feedback: Some("reconnecting".to_owned()),
    };
    let projected = super::director_drawer_projection(&ui, &runtime, Some(&terminal_view));
    assert_eq!(projected.conversations.len(), 2);
    assert!(projected.conversations[0].selected);
    assert!(!projected.organization.is_empty());
    assert_eq!(projected.organization[0].label, "♛ Director");
    assert_eq!(projected.terminal_view, Some(terminal_view));
    assert_eq!(projected.interrupted_detail, None);

    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let goal_projection = super::director_drawer_projection(&ui, &runtime, None);
    assert!(goal_projection.conversations.is_empty());
    assert!(goal_projection.organization.is_empty());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);

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
        super::director_drawer_projection(&ui, &runtime, Some(&quiet_terminal_view)).terminal_view,
        Some(quiet_terminal_view)
    );

    // Closing the selected live Agent is a no-op. The daemon-owned tab and
    // selection remain intact until the CLI exits.
    let mut pending_targets = std::collections::HashMap::new();
    super::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending_targets);
    let interrupted_projection = super::director_drawer_projection(&ui, &runtime, None);
    assert!(interrupted_projection.conversations[0].selected);
    assert_eq!(interrupted_projection.interrupted_detail, None);
    assert_eq!(interrupted_projection.terminal_view, None);
    runtime.fail_tab_resume_for(
        Target::Root(workspace),
        interrupted_continuation,
        None,
        "safe retry feedback".to_owned(),
    );
    let failed_projection = super::director_drawer_projection(&ui, &runtime, None);
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
        super::director_drawer_projection(&ui, &runtime, Some(&live_failure_view));
    assert_eq!(
        live_failure_projection.feedback.as_deref(),
        Some("safe retry feedback"),
        "management routes keep launch failure feedback beside an older live Director"
    );
    assert!(super::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    let selected_interrupted = super::director_drawer_projection(&ui, &runtime, None);
    assert!(selected_interrupted.conversations[1].selected);
    assert!(selected_interrupted.interrupted_detail.is_some());
    assert!(super::select_director_tab_and_activate(
        &Key::Live(LiveTerminalAction::PreviousTab),
        &mut ui,
        &mut runtime,
        &mut pending_targets,
    ));
    assert_eq!(runtime.focused_terminal(), Some(live.clone()));
    assert!(super::select_director_tab(
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
    let pending_projection = super::director_drawer_projection(&ui, &runtime, None);
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
    assert!(super::select_director_tab(
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
    let filtered = super::director_drawer_projection(&ui, &runtime, None);
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
    assert!(!super::select_director_tab(
        &Key::Char('x'),
        &mut ui,
        &mut runtime,
    ));
    assert!(!super::select_director_tab(
        &Key::Live(LiveTerminalAction::OpenPullRequests),
        &mut ui,
        &mut runtime,
    ));
    assert!(super::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    assert!(super::select_director_tab(
        &Key::Live(LiveTerminalAction::PreviousTab),
        &mut ui,
        &mut runtime,
    ));
    assert!(super::select_director_tab(
        &Key::Down,
        &mut ui,
        &mut runtime,
    ));
    assert!(super::select_director_tab(&Key::Up, &mut ui, &mut runtime,));
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
    let ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::SakanaAi]),
        DefaultModel::SakanaAi,
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
    assert_eq!(
        super::director_drawer_projection(&ui, &runtime, None).new,
        super::DirectorNewProjection::Choosing {
            candidates: vec!["claude".to_owned(), "sakana.ai".to_owned()],
            selected: 1,
        }
    );

    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let _ = runtime.handle_key(Key::Char('g'));
    let _ = runtime.handle_key(Key::Paste("o".to_owned()));
    let _ = runtime.handle_key(Key::Backspace);
    let goal_projection = super::director_drawer_projection(&ui, &runtime, None);
    assert!(goal_projection.goal_driven);
    assert!(matches!(
        goal_projection.new,
        super::DirectorNewProjection::GoalComposer {
            selected: 1,
            ref goal,
            ..
        } if goal == "g"
    ));
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);

    let _ = runtime.handle_key(Key::Escape);
    runtime.set_agent_models(AvailableModels::default(), DefaultModel::OpenAi);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::DirectorNew));
    assert_eq!(
        super::director_drawer_projection(&ui, &runtime, None).new,
        super::DirectorNewProjection::Empty
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
        super::director_drawer_projection(&ui, &runtime, None).new,
        super::DirectorNewProjection::Launching
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
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
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
        vec![super::PaneRestoreTarget {
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
    assert!(!super::select_director_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut ui,
        &mut runtime,
    ));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(super::select_director_tab(
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
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
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
        vec![super::PaneRestoreTarget {
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
fn root_terminal_pointer_and_wheel_use_the_bottom_drawer_viewport() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Terminal);
    let _ = runtime.complete_pane(Target::Root(workspace), operation, terminal);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut pending = std::collections::HashMap::new();
    let drawer = crate::presentation::views::root_terminal_drawer::geometry(20, 80);
    let row = u16::try_from(drawer.top + 2).unwrap();

    assert!(!handle_terminal_pointer(
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
            column: 2,
            row,
        },
    ));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::Wheel {
            up: true,
            column: 2,
            row,
            notches: 1,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        0,
        0,
    ));
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabNext),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        0,
        0,
    ));
}

#[test]
fn root_generic_host_request_is_admitted_and_untracked_resume_completion_is_inert() {
    let workspace = WorkspaceId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let mut pending = std::collections::HashMap::new();
    let (mut host, actions) = ControllerHost::channel();
    let operation = OperationId::new();
    super::BackendAgentPort::open_terminal(
        &mut host,
        crate::usecase::application::daemon_backend::OpenTerminalRequest {
            target: Target::Root(workspace),
            operation_id: operation,
            arguments: "new".to_owned(),
        },
    );
    drain_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
    assert_eq!(pending.get(&operation), Some(&Target::Root(workspace)));
    assert_eq!(ui.pane_launches.len(), 1);
    assert!(
            runtime
                .panes()
                .pane(Target::Root(workspace))
                .unwrap()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Pending(pending) if pending.operation == operation && pending.kind == PaneKind::Terminal))
        );

    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::ResumeExact {
                operation: OperationId::new(),
                continuation: AgentContinuationRef::new(),
                result: Err("late answer".to_owned()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );
    assert!(
        runtime
            .panes()
            .pane(Target::Root(workspace))
            .unwrap()
            .tabs()
            .iter()
            .any(|tab| matches!(tab, PaneTab::Pending(pending) if pending.operation == operation))
    );
}

#[test]
fn restore_without_agent_intent_caches_inventory_and_refresh_clears_it() {
    let workspace = WorkspaceId::new();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let fence = runtime.restore_fence();
    let applied = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::new(),
            terminals: Ok(Vec::new()),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::new(),
    );
    assert_eq!(applied.outcome, super::RestoreJobOutcome::Applied);
    assert_eq!(
        ui.agent_inventory().map(|inventory| inventory.workspace_id),
        Some(workspace)
    );
    ui.refresh_agent_inventory();
    assert!(ui.agent_inventory().is_none());
    assert!(ui.take_agent_observation_request());
    assert!(runtime.active_pane().tabs().is_empty());
}

#[test]
fn close_tab_live_action_cancels_the_focused_pending_launch() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let target = Target::Session(session);
    let live = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        live.clone(),
        Box::new(ScriptedAgentPort {
            terminal: live.clone(),
            subscription: 19,
            replay: Vec::new(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let operation = OperationId::new();
    let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
    let _ = runtime.select_tab(crate::usecase::application::controller::TabDirection::Next);
    ui.pane_launches.push(PaneLaunch::Terminal {
        operation: OperationId::new(),
        workspace,
        session: Some(session),
        arguments: "open".to_owned(),
    });
    ui.pane_launches.push(PaneLaunch::Agent {
        operation,
        workspace,
        session: Some(session),
        profile: None,
        goal: None,
        resume: false,
    });
    let mut pending_targets = std::collections::HashMap::from([(operation, target)]);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
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

    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(runtime.focused_terminal(), Some(live));
    assert!(!pending_targets.contains_key(&operation));
    assert!(matches!(
        ui.pane_launches.as_slice(),
        [PaneLaunch::Terminal { .. }]
    ));

    let unqueued = OperationId::new();
    let _ = runtime.request_pane(target, unqueued, PaneKind::Terminal);
    let _ = runtime.select_tab(TabDirection::Next);
    pending_targets.insert(unqueued, target);
    super::close_focused_terminal_pane(&mut ui, &mut runtime, &mut pending_targets);
    assert!(!pending_targets.contains_key(&unqueued));
    assert!(matches!(
        ui.pane_launches.as_slice(),
        [PaneLaunch::Terminal { .. }]
    ));

    // Closeup still permits dismissing its only pending tab. Switch blocks
    // the same control before it can reach this pane mutation path.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut pending_ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut pending_runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = pending_runtime.handle_key(Key::Down);
    let _ = pending_runtime.handle_key(Key::Enter);
    let operation = OperationId::new();
    let _ = pending_runtime.request_pane(target, operation, PaneKind::Terminal);
    let _ = pending_runtime.select_tab(TabDirection::Next);
    let mut pending_targets = std::collections::HashMap::from([(operation, target)]);
    super::close_focused_terminal_pane(&mut pending_ui, &mut pending_runtime, &mut pending_targets);
    assert!(pending_runtime.active_pane().tabs().is_empty());
    assert!(pending_targets.is_empty());
}

/// A daemon inventory double for restore-on-open. It returns a fixed set of
/// in-scope runtimes and attaches successfully so a restored tab streams.
type RecordedTerminalInputs = Arc<Mutex<Vec<(TerminalRef, Vec<u8>)>>>;

struct RestoreInventoryPort {
    entries: Vec<TerminalInventoryEntry>,
    fail: bool,
    inputs: RecordedTerminalInputs,
}
#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=terminal_restore_fake_port_contract
impl AgentCommandPort for RestoreInventoryPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("restore never launches".to_owned())
    }
    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        if self.fail {
            Err(TerminalError::Unavailable)
        } else {
            Ok(self.entries.clone())
        }
    }
    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Ok(TerminalAttach {
            subscription: TerminalSubscription { id: 1, epoch: 1 },
            revision: 1,
            output_offset: 0,
            next_input_seq: None,
            screen: attach_checkpoint(&[], geometry),
            exited: false,
        })
    }
    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        Ok(Vec::new())
    }
    fn input_terminal(
        &mut self,
        terminal: &TerminalRef,
        _subscription: TerminalSubscription,
        _input_seq: u64,
        _operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        self.inputs
            .lock()
            .unwrap()
            .push((terminal.clone(), bytes.to_vec()));
        Ok(TerminalInputOutcome::Written)
    }
}

struct RetryRestorePort {
    workspace: WorkspaceId,
    entries: Vec<TerminalInventoryEntry>,
    runtimes: Vec<AgentRuntimeInventoryItem>,
    fail_attempts: usize,
    terminal_attempts: Arc<AtomicUsize>,
    agent_attempts: Arc<AtomicUsize>,
}

struct SequencedRestorePort {
    terminals: VecDeque<Result<Vec<TerminalInventoryEntry>, TerminalError>>,
    agents: VecDeque<Result<AgentInventory, String>>,
}

impl AgentCommandPort for SequencedRestorePort {
    fn launch(
        &mut self,
        _: OperationId,
        _: WorkspaceId,
        _: Option<SessionId>,
        _: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        panic!("restore observation must never launch an Agent")
    }

    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        self.terminals
            .pop_front()
            .expect("terminal observation script exhausted")
    }

    fn resume_inventory(&mut self, _: WorkspaceId) -> Result<AgentInventory, String> {
        self.agents
            .pop_front()
            .expect("Agent observation script exhausted")
    }
}

impl AgentCommandPort for RetryRestorePort {
    fn launch(
        &mut self,
        _: OperationId,
        _: WorkspaceId,
        _: Option<SessionId>,
        _: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        panic!("restore must never launch an Agent")
    }

    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        if self.terminal_attempts.fetch_add(1, Ordering::SeqCst) < self.fail_attempts {
            Err(TerminalError::Unavailable)
        } else {
            Ok(self.entries.clone())
        }
    }

    fn resume_inventory(&mut self, workspace: WorkspaceId) -> Result<AgentInventory, String> {
        assert_eq!(workspace, self.workspace);
        if self.agent_attempts.fetch_add(1, Ordering::SeqCst) < self.fail_attempts {
            Err("temporary inventory failure".to_owned())
        } else {
            Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: self.runtimes.clone(),
                resumable: Vec::new(),
            })
        }
    }
}

struct MemoryIntentPort {
    state: Arc<Mutex<AgentTabIntent>>,
    mutations: Arc<Mutex<Vec<AgentTabIntentMutation>>>,
}

struct FailingIntentPort {
    state: Arc<Mutex<AgentTabIntent>>,
    error: AgentTabIntentError,
    attempts: Arc<AtomicUsize>,
}

struct LoadFailingIntentPort;

impl AgentTabIntentPort for LoadFailingIntentPort {
    fn load(&mut self, _workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
        Err(AgentTabIntentError::ReadOnlySchema)
    }

    fn mutate(
        &mut self,
        _workspace: WorkspaceId,
        _expected_revision: u64,
        _mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
        Err(AgentTabIntentError::ReadOnlySchema)
    }
}

impl AgentTabIntentPort for FailingIntentPort {
    fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
        let state = self.state.lock().unwrap();
        assert_eq!(workspace, state.workspace_id);
        Ok(state.clone())
    }

    fn mutate(
        &mut self,
        workspace: WorkspaceId,
        _expected_revision: u64,
        _mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
        assert_eq!(workspace, self.state.lock().unwrap().workspace_id);
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(self.error)
    }
}

impl AgentTabIntentPort for MemoryIntentPort {
    fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
        let state = self.state.lock().unwrap();
        assert_eq!(workspace, state.workspace_id);
        Ok(state.clone())
    }

    fn mutate(
        &mut self,
        workspace: WorkspaceId,
        expected_revision: u64,
        mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
        let mut state = self.state.lock().unwrap();
        assert_eq!(workspace, state.workspace_id);
        self.mutations.lock().unwrap().push(mutation.clone());
        let commit = reconcile_agent_tab_intent_mutation(
            state.clone(),
            workspace,
            expected_revision,
            mutation,
        )?;
        *state = commit.intent.clone();
        Ok(commit)
    }
}

#[test]
fn memory_intent_port_fences_an_idempotent_close_before_reopen() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let mut durable = AgentTabIntent::empty(workspace);
    durable.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation,
        terminal: scoped_terminal_ref(workspace, None),
        select: true,
    });
    durable.revision = 1;
    durable.apply(AgentTabIntentMutation::Dismiss { continuation });
    durable.revision = 2;
    let shared = Arc::new(Mutex::new(durable));
    let mut port = MemoryIntentPort {
        state: Arc::clone(&shared),
        mutations: Arc::new(Mutex::new(Vec::new())),
    };

    let close = port
        .mutate(
            workspace,
            1,
            AgentTabIntentMutation::Dismiss { continuation },
        )
        .unwrap();
    assert!(close.cas_conflict);
    assert_eq!(close.intent.revision, 3);

    let reopen = port
        .mutate(
            workspace,
            2,
            AgentTabIntentMutation::Reopen { continuation },
        )
        .unwrap();
    assert!(reopen.cas_conflict);
    assert!(!reopen.mutation_applied);
    assert!(reopen.intent.dismissed.contains(&continuation));
    assert_eq!(reopen.intent.revision, 3);
    assert_eq!(shared.lock().unwrap().revision, 3);

    // A deferred close merges the same way: the exact fence is recorded
    // under the conflict and a repeat still advances the revision.
    let unobserved = scoped_terminal_ref(workspace, None);
    let deferred = port
        .mutate(
            workspace,
            1,
            AgentTabIntentMutation::DismissTerminalAndSelect {
                terminal: unobserved.clone(),
                session_id: None,
                selected: Some(continuation),
            },
        )
        .unwrap();
    assert!(deferred.cas_conflict);
    assert!(deferred.intent.dismissed_terminals.contains(&unobserved));
    assert_eq!(deferred.intent.revision, 4);
    let repeated = port
        .mutate(
            workspace,
            deferred.intent.revision,
            AgentTabIntentMutation::DismissTerminal {
                terminal: unobserved,
            },
        )
        .unwrap();
    assert!(!repeated.cas_conflict);
    assert_eq!(repeated.intent.revision, 5);
}

#[test]
fn memory_intent_port_projects_a_stale_observation_without_mutating_state() {
    let workspace = WorkspaceId::new();
    let mut durable = AgentTabIntent::empty(workspace);
    durable.revision = 1;
    let mut port = MemoryIntentPort {
        state: Arc::new(Mutex::new(durable)),
        mutations: Arc::new(Mutex::new(Vec::new())),
    };
    let inventory = AgentInventory {
        workspace_id: workspace,
        runtimes: Vec::new(),
        resumable: Vec::new(),
    };

    let commit = port
        .mutate(
            workspace,
            0,
            AgentTabIntentMutation::Observe {
                terminals: Vec::new(),
                agents: inventory,
                allowed_sessions: BTreeSet::new(),
            },
        )
        .unwrap();
    assert!(commit.cas_conflict);
    assert!(!commit.mutation_applied);
    assert_eq!(commit.projection, Some(AgentTabProjection::default()));
}

#[test]
fn unavailable_and_load_failing_intent_ports_keep_typed_fallback_state() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let mut unavailable = super::UnavailableAgentTabIntentPort;
    assert_eq!(
        unavailable.load(workspace).unwrap(),
        AgentTabIntent::empty(workspace)
    );
    let committed = unavailable
        .mutate(
            workspace,
            0,
            AgentTabIntentMutation::Upsert {
                session_id: None,
                continuation,
                terminal,
                select: true,
            },
        )
        .unwrap();
    assert!(committed.mutation_applied);
    assert!(!committed.cas_conflict);
    assert_eq!(committed.intent.targets[0].selected, Some(continuation));

    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let observation = ui
        .observe_agent_tabs(
            Vec::new(),
            AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        )
        .unwrap();
    assert!(observation.cas_accepted);
    assert_eq!(observation.projection, AgentTabProjection::default());
    ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss { continuation })
        .unwrap();

    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(workspace, BTreeSet::new(), Box::new(LoadFailingIntentPort));
    assert_eq!(
        ui.take_agent_tab_intent_load_error(),
        Some(AgentTabIntentError::ReadOnlySchema)
    );
    assert_eq!(ui.take_agent_tab_intent_load_error(), None);
}

fn scoped_terminal_ref(workspace: WorkspaceId, session: Option<SessionId>) -> TerminalRef {
    TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: session,
        worktree_id: WorktreeId::new(),
    }
}

#[test]
fn restore_worker_retries_both_inventories_without_launching() {
    let workspace = WorkspaceId::new();
    let terminal_attempts = Arc::new(AtomicUsize::new(0));
    let agent_attempts = Arc::new(AtomicUsize::new(0));
    let terminal = scoped_terminal_ref(workspace, None);
    let (sender, receiver) = std::sync::mpsc::channel();

    super::spawn_restore_job(
        Box::new(RetryRestorePort {
            workspace,
            entries: vec![TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            }],
            runtimes: Vec::new(),
            fail_attempts: 2,
            terminal_attempts: Arc::clone(&terminal_attempts),
            agent_attempts: Arc::clone(&agent_attempts),
        }),
        workspace,
        BTreeSet::new(),
        7,
        11,
        sender,
    );

    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("bounded restore retry completes");
    assert_eq!(completion.dispatched_interaction, 7);
    assert_eq!(completion.dispatched_registry_revision, 11);
    assert_eq!(completion.terminals.unwrap()[0].terminal, terminal);
    assert_eq!(completion.agents.unwrap().workspace_id, workspace);
    assert!(completion.observation_coherent);
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 6);
    assert_eq!(agent_attempts.load(Ordering::SeqCst), 3);
}

#[test]
fn restore_worker_retries_a_cross_rpc_snapshot_race_until_refs_are_coherent() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let old = scoped_terminal_ref(workspace, None);
    let replacement = scoped_terminal_ref(workspace, None);
    let entry = |terminal: &TerminalRef| TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    };
    let inventory = |terminal: &TerminalRef| AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), None).unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    super::spawn_restore_job(
        Box::new(SequencedRestorePort {
            // First terminal/Agent/terminal bracket races O -> R. The
            // second bracket is stable at R and is the only accepted one.
            terminals: VecDeque::from([
                Ok(vec![entry(&old)]),
                Ok(vec![entry(&replacement)]),
                Ok(vec![entry(&replacement)]),
                Ok(vec![entry(&replacement)]),
            ]),
            agents: VecDeque::from([Ok(inventory(&old)), Ok(inventory(&replacement))]),
        }),
        workspace,
        BTreeSet::new(),
        0,
        0,
        sender,
    );

    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(completion.observation_coherent);
    assert_eq!(
        completion.terminals.as_ref().unwrap()[0].terminal,
        replacement
    );
    assert!(
        completion.agents.as_ref().unwrap().runtimes[0]
            .runtime
            .terminal
            .fences(&replacement)
    );
}

#[test]
fn restore_worker_rejects_an_agent_inventory_from_another_workspace() {
    let workspace = WorkspaceId::new();
    let wrong_inventory = AgentInventory {
        workspace_id: WorkspaceId::new(),
        runtimes: Vec::new(),
        resumable: Vec::new(),
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    super::spawn_restore_job(
        Box::new(SequencedRestorePort {
            terminals: VecDeque::from([
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
                Ok(Vec::new()),
            ]),
            agents: VecDeque::from([
                Ok(wrong_inventory.clone()),
                Ok(wrong_inventory.clone()),
                Ok(wrong_inventory),
            ]),
        }),
        workspace,
        BTreeSet::new(),
        0,
        0,
        sender,
    );

    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(!completion.observation_coherent);
    assert_eq!(
        completion.agents.unwrap_err(),
        "Agent inventory scope changed while restoring"
    );
}

#[test]
fn partial_transport_failure_restores_nothing_and_outranks_a_stale_fence() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let generic = scoped_terminal_ref(workspace, Some(session));
    let mut initial_intent = AgentTabIntent::empty(workspace);
    initial_intent.revision = 3;
    let durable = Arc::new(Mutex::new(initial_intent));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let dispatched = runtime.restore_fence();
    let runtime_before = runtime.active_pane().clone();
    let partial = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: dispatched.0,
            dispatched_registry_revision: dispatched.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: generic.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            }]),
            agents: Err("Agent inventory unavailable".to_owned()),
            observation_coherent: false,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(partial.outcome, super::RestoreJobOutcome::TransportFailed);
    assert_eq!(runtime.active_pane(), &runtime_before);
    assert_ne!(runtime.focused_terminal(), Some(generic.clone()));
    assert!(mutations.lock().unwrap().is_empty());
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );

    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(retry.complete(std::time::Duration::ZERO, partial.outcome));
    assert!(!retry.begin_if_due(std::time::Duration::from_millis(249)));
    assert!(retry.begin_if_due(std::time::Duration::from_millis(250)));

    // User activity advances the runtime fence while the next partial
    // request is in flight. Transport failure still wins and advances the
    // outage backoff instead of immediately redispatching.
    let _ = runtime.handle_key(Key::Down);
    let both_failed = super::apply_restore_completion(
        super::RestoreCompletion {
            port: partial.port,
            dispatched_interaction: dispatched.0,
            dispatched_registry_revision: dispatched.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: generic,
                kind: TerminalKind::Terminal,
                live: true,
            }]),
            agents: Err("Agent inventory unavailable".to_owned()),
            observation_coherent: false,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(
        both_failed.outcome,
        super::RestoreJobOutcome::TransportFailed
    );
    assert!(mutations.lock().unwrap().is_empty());
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    assert!(!retry.complete(std::time::Duration::from_millis(250), both_failed.outcome));
    assert!(!retry.begin_if_due(std::time::Duration::from_millis(749)));
    assert!(retry.begin_if_due(std::time::Duration::from_millis(750)));
}

#[test]
fn reconnect_racing_an_in_flight_restore_schedules_one_fresh_observation() {
    let now = std::time::Duration::from_secs(7);
    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    retry.reconnected(1, now);
    assert_eq!(retry.followup, super::RestoreFollowup::Reconnected);
    assert!(!retry.complete(now, super::RestoreJobOutcome::Applied));
    assert_eq!(retry.followup, super::RestoreFollowup::None);
    assert_eq!(retry.failures, 0);
    assert_eq!(retry.next_retry_at, Some(now));
    assert!(retry.begin_if_due(now));
    assert!(!retry.begin_if_due(now));
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers scope fencing and duplicate normalization.
fn restore_scope_change_rejects_snapshot_and_exact_duplicates_normalize_once() {
    let workspace = WorkspaceId::new();
    let original_session = SessionId::new();
    let added_session = SessionId::new();
    let terminal = scoped_terminal_ref(workspace, Some(original_session));
    let entry = TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Terminal,
        live: true,
    };
    let same_terminal_agent = TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    };
    let mut duplicated = vec![entry.clone(), same_terminal_agent.clone(), entry.clone()];
    super::normalize_terminal_inventory(&mut duplicated);
    assert_eq!(duplicated, vec![same_terminal_agent, entry.clone()]);
    let generic_only = vec![entry.clone()];
    assert!(super::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &generic_only,
        &AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));
    assert!(!super::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &generic_only,
        &AgentInventory {
            workspace_id: WorkspaceId::new(),
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));

    let out_of_scope_terminal = scoped_terminal_ref(workspace, Some(added_session));
    assert!(!super::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &[TerminalInventoryEntry {
            terminal: out_of_scope_terminal,
            kind: TerminalKind::Terminal,
            live: true,
        }],
        &AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));

    let mut conflicting = generic_only.clone();
    conflicting.push(TerminalInventoryEntry {
        terminal: terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    });
    assert!(!super::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &conflicting,
        &AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
    ));

    let foreign = scoped_terminal_ref(workspace, Some(added_session));
    let continuation = AgentContinuationRef::new();
    let foreign_runtime = AgentRuntimeInventoryItem {
        runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), foreign, Some(added_session)).unwrap(),
        continuation,
        state: AgentRuntimeInventoryState::Live,
        resumed_from: None,
    };
    assert!(!super::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &generic_only,
        &AgentInventory {
            workspace_id: workspace,
            runtimes: vec![foreign_runtime],
            resumable: Vec::new(),
        },
    ));

    let agent_terminal = scoped_terminal_ref(workspace, Some(original_session));
    let agent_entry = TerminalInventoryEntry {
        terminal: agent_terminal.clone(),
        kind: TerminalKind::Agent,
        live: true,
    };
    let duplicate_runtime = || AgentRuntimeInventoryItem {
        runtime: AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            agent_terminal.clone(),
            Some(original_session),
        )
        .unwrap(),
        continuation,
        state: AgentRuntimeInventoryState::Live,
        resumed_from: None,
    };
    assert!(!super::restore_inventory_is_coherent(
        workspace,
        &BTreeSet::from([original_session]),
        &[agent_entry],
        &AgentInventory {
            workspace_id: workspace,
            runtimes: vec![duplicate_runtime(), duplicate_runtime()],
            resumable: Vec::new(),
        },
    ));

    let mut view_state = state("demo");
    let mut added_record = view_state.sessions[0].clone();
    added_record.name = "added-session".to_owned();
    view_state.sessions.push(added_record);
    let view = WorkspaceView::with_runtime_ids(
        ws("demo"),
        view_state,
        vec![original_session, added_session],
    );
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![original_session, added_session]);
    let fence = runtime.restore_fence();
    let applied = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([original_session]),
            terminals: Ok(vec![entry]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([original_session, added_session]),
    );
    assert_eq!(applied.outcome, super::RestoreJobOutcome::FenceRejected);
    assert!(
        runtime
            .panes()
            .pane(Target::Session(original_session))
            .is_some_and(|pane| pane.tabs().is_empty())
    );
    assert_ne!(runtime.focused_terminal(), Some(terminal));
}

#[test]
fn restore_intent_publish_failure_keeps_bytes_but_does_not_block_generic_restore() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let generic = scoped_terminal_ref(workspace, Some(session));
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&durable),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    let applied = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: generic.clone(),
                kind: TerminalKind::Terminal,
                live: true,
            }]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(
        applied.outcome,
        super::RestoreJobOutcome::IntentFailed(AgentTabIntentError::Unavailable)
    );
    assert!(matches!(
        runtime.active_pane().tabs(),
        [PaneTab::Live(LivePane {
            terminal,
            kind: PaneKind::Terminal
        })] if terminal.fences(&generic)
    ));
    assert_eq!(runtime.focused_terminal(), Some(generic));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(!retry.complete(std::time::Duration::ZERO, applied.outcome));
    assert!(!retry.begin_if_due(std::time::Duration::from_secs(60)));
    if let super::RestoreJobOutcome::IntentFailed(error) = applied.outcome {
        super::surface_agent_tab_intent_error(&mut runtime, error);
    }
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Mixed inventory and prior runtime state share one failure fixture.
fn mixed_restore_intent_failure_preserves_visible_agents_and_restores_generics() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let inventory_only_continuation = AgentContinuationRef::new();
    let agent = scoped_terminal_ref(workspace, Some(session));
    let inventory_only_agent = scoped_terminal_ref(workspace, Some(session));
    let existing_generic = scoped_terminal_ref(workspace, Some(session));
    let new_generic = scoped_terminal_ref(workspace, Some(session));
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: agent.clone(),
        select: true,
    });
    intent.revision = 3;
    let durable = Arc::new(Mutex::new(intent));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&durable),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: agent.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: existing_generic.clone(),
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(agent.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let fence = runtime.restore_fence();
    let applied = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![
                TerminalInventoryEntry {
                    terminal: agent.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: inventory_only_agent.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: existing_generic.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: new_generic.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                },
            ]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: vec![
                    AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            agent.clone(),
                            Some(session),
                        )
                        .unwrap(),
                        continuation,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    },
                    AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            inventory_only_agent.clone(),
                            Some(session),
                        )
                        .unwrap(),
                        continuation: inventory_only_continuation,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    },
                ],
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(
        applied.outcome,
        super::RestoreJobOutcome::IntentFailed(AgentTabIntentError::Unavailable)
    );
    assert!(matches!(
        runtime.active_pane().tabs(),
        [
            PaneTab::Live(LivePane { terminal: visible_agent, kind: PaneKind::Agent }),
            PaneTab::Live(LivePane { terminal: retained_generic, kind: PaneKind::Terminal }),
            PaneTab::Live(LivePane { terminal: added_generic, kind: PaneKind::Terminal })
        ] if visible_agent.fences(&agent)
            && retained_generic.fences(&existing_generic)
            && added_generic.fences(&new_generic)
    ));
    assert!(runtime.active_pane().tabs().iter().all(|tab| {
        !matches!(tab, PaneTab::Live(pane) if pane.terminal.fences(&inventory_only_agent))
    }));
    assert_eq!(runtime.focused_terminal(), Some(agent));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    if let super::RestoreJobOutcome::IntentFailed(error) = applied.outcome {
        super::surface_agent_tab_intent_error(&mut runtime, error);
    }
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );
}

/// The Garden's cross-project lane observes only while the screen saver is
/// on screen, keeps one round in flight at a time, and backs off when the
/// daemon answers nothing. Closing the Garden re-arms it, so the next
/// opening shows the other projects' Agents without waiting out a cadence.
#[test]
fn garden_observation_runs_only_while_the_garden_is_open_and_backs_off_unanswered() {
    let mut lane = super::ObservationLane::new(
        super::GARDEN_OBSERVATION_INTERVAL,
        super::GARDEN_OBSERVATION_BACKOFF,
    );
    let mut now = std::time::Duration::ZERO;
    for _ in 0..1_000 {
        assert!(
            !lane.begin_if_due(false, now),
            "a closed Garden observes nothing"
        );
        now += std::time::Duration::from_millis(16);
    }

    assert!(lane.begin_if_due(true, now), "opening observes immediately");
    assert!(
        !lane.begin_if_due(true, now),
        "one round holds the only port"
    );
    lane.complete(now, true);
    assert!(!lane.begin_if_due(true, now));
    let just_before = (now + super::GARDEN_OBSERVATION_INTERVAL)
        .checked_sub(std::time::Duration::from_millis(1))
        .expect("the cadence is longer than a millisecond");
    assert!(!lane.begin_if_due(true, just_before));
    assert!(lane.begin_if_due(true, now + super::GARDEN_OBSERVATION_INTERVAL));

    // Nothing answered: the same open Garden waits out the longer backoff.
    lane.complete(now, false);
    assert!(!lane.begin_if_due(true, now + super::GARDEN_OBSERVATION_INTERVAL * 4));
    assert!(lane.begin_if_due(true, now + super::GARDEN_OBSERVATION_BACKOFF));

    // A round dispatched before the Garden closed still owns the port, so
    // closing does not admit a second one; once it lands, re-opening
    // observes at once.
    assert!(!lane.begin_if_due(false, now));
    lane.complete(now, true);
    assert!(!lane.begin_if_due(false, now));
    assert!(lane.begin_if_due(true, now));
}

#[test]
fn work_run_observation_is_single_flight_and_bounded() {
    let mut lane = super::ObservationLane::new(
        super::WORK_RUN_OBSERVATION_INTERVAL,
        super::WORK_RUN_OBSERVATION_BACKOFF,
    );
    let now = std::time::Duration::from_secs(1);
    lane.refresh_now();
    assert!(lane.begin_if_due(true, now));
    lane.refresh_now();
    assert!(!lane.begin_if_due(true, now));
    lane.complete(now, true);
    assert!(!lane.begin_if_due(true, now + super::WORK_RUN_OBSERVATION_INTERVAL / 2));
    let next = now + super::WORK_RUN_OBSERVATION_INTERVAL;
    assert!(lane.begin_if_due(true, next));
    lane.complete(next, false);
    assert!(!lane.begin_if_due(true, next + super::WORK_RUN_OBSERVATION_BACKOFF / 2));
    lane.refresh_now();
    assert!(lane.begin_if_due(true, next));
    lane.complete(next, false);
    assert!(lane.begin_if_due(true, next + super::WORK_RUN_OBSERVATION_BACKOFF));
}

#[test]
fn unavailable_work_run_port_fails_observation_and_control_closed() {
    use super::WorkRunPort as _;

    let workspace = WorkspaceId::new();
    let mut port = super::UnavailableWorkRunPort;
    assert_eq!(
        port.snapshot(workspace).unwrap_err(),
        "Work Run progress is unavailable"
    );
    let command = usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Cancel {
        supervisor_run_id: usagi_core::domain::supervisor::SupervisorRunId::new(),
        reason: "operator cancelled".into(),
    };
    assert_eq!(
        port.control(workspace, OperationId::new(), command)
            .unwrap_err(),
        super::WorkRunControlError::Rejected(
            "Work Run action is unavailable; refresh and try again".into()
        )
    );
}

#[test]
fn work_run_observation_drops_a_mismatched_workspace() {
    struct MismatchedWorkRuns;

    impl super::WorkRunPort for MismatchedWorkRuns {
        fn snapshot(
            &mut self,
            _: WorkspaceId,
        ) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
            Ok(
                usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                    workspace_id: WorkspaceId::new(),
                    runs: Vec::new(),
                },
            )
        }

        fn control(
            &mut self,
            _: WorkspaceId,
            _: OperationId,
            _: usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
        ) -> Result<super::WorkRunControlResult, super::WorkRunControlError> {
            unreachable!("observation test never controls a Work Run")
        }
    }

    let requested = WorkspaceId::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    super::spawn_work_run_observation_job(Box::new(MismatchedWorkRuns), requested, sender);
    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the Work Run observation returns its port");
    let super::WorkRunLaneCompletion::Observation { snapshot, .. } = completion else {
        panic!("observation job returned a control completion");
    };
    assert_eq!(
        snapshot.unwrap_err(),
        "daemon returned another workspace's Work Runs"
    );
}

#[test]
fn work_run_lane_rejects_unbounded_or_private_snapshots() {
    let workspace = WorkspaceId::new();
    let run = observed_work_run(SupervisorRunState::Running);
    assert_eq!(
        super::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: vec![run.clone(), run.clone()],
            },
            workspace,
        )
        .unwrap_err(),
        "daemon returned invalid Work Run progress"
    );
    let too_many = (0..=super::MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS)
        .map(|_| observed_work_run(SupervisorRunState::Running))
        .collect();
    assert_eq!(
        super::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: too_many,
            },
            workspace,
        )
        .unwrap_err(),
        "daemon returned invalid Work Run progress"
    );
    assert_eq!(
        super::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: vec![with_private_work_run_provenance(run.clone())],
            },
            workspace,
        )
        .unwrap_err(),
        "daemon returned invalid Work Run progress"
    );
    assert!(
        super::validate_work_run_snapshot(
            usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot {
                workspace_id: workspace,
                runs: vec![run.clone()],
            },
            workspace,
        )
        .is_ok()
    );
}

#[test]
fn work_run_lane_rejects_mismatched_or_private_control_results() {
    let workspace = WorkspaceId::new();
    let run = observed_work_run(SupervisorRunState::Running);
    let request = super::WorkRunControlRequest {
        operation_id: OperationId::new(),
        command: usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: run.supervisor_run_id,
            reason: "operator cancelled".into(),
        },
        observed_state_revision: run.state_revision,
    };
    let (operation_id, result) = complete_work_run_control(
        workspace,
        request.clone(),
        super::WorkRunControlResult::Updated(Box::new(observed_work_run(
            SupervisorRunState::Cancelled,
        ))),
    );
    assert_eq!(operation_id, request.operation_id);
    assert_eq!(
        result.unwrap_err(),
        super::WorkRunControlError::Unconfirmed(
            "daemon returned an invalid Work Run result".to_owned()
        )
    );

    let (_, result) = complete_work_run_control(
        workspace,
        request,
        super::WorkRunControlResult::Updated(Box::new(with_private_work_run_provenance(
            run.clone(),
        ))),
    );
    assert_eq!(
        result.unwrap_err(),
        super::WorkRunControlError::Unconfirmed(
            "daemon returned an invalid Work Run result".to_owned()
        )
    );
    let deletion = usagi_core::domain::supervisor::SupervisorRunDeletion {
        supervisor_run_id: SupervisorRunId::new(),
        state_revision: 4,
    };
    let request = super::WorkRunControlRequest {
        operation_id: OperationId::new(),
        command: usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Delete {
            supervisor_run_id: deletion.supervisor_run_id,
            observed_state_revision: deletion.state_revision,
        },
        observed_state_revision: deletion.state_revision,
    };
    let (_, result) = complete_work_run_control(
        workspace,
        request,
        super::WorkRunControlResult::Deleted(deletion),
    );
    assert_eq!(result, Ok(super::WorkRunControlResult::Deleted(deletion)));
}

#[test]
fn work_run_lane_recovers_ports_after_adapter_panics() {
    struct PanickingWorkRuns;
    impl super::WorkRunPort for PanickingWorkRuns {
        fn snapshot(
            &mut self,
            _: WorkspaceId,
        ) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
            panic!("snapshot adapter panic")
        }

        fn control(
            &mut self,
            _: WorkspaceId,
            _: OperationId,
            _: usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
        ) -> Result<super::WorkRunControlResult, super::WorkRunControlError> {
            panic!("control adapter panic")
        }
    }

    let workspace = WorkspaceId::new();
    let run = observed_work_run(SupervisorRunState::Running);
    let (sender, receiver) = std::sync::mpsc::channel();
    super::spawn_work_run_observation_job(Box::new(PanickingWorkRuns), workspace, sender);
    let super::WorkRunLaneCompletion::Observation { snapshot, .. } = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a panicking observation still returns its port")
    else {
        panic!("observation job returned a control completion");
    };
    assert_eq!(
        snapshot.unwrap_err(),
        "Work Run progress is temporarily unavailable"
    );

    let operation_id = OperationId::new();
    let request = super::WorkRunControlRequest {
        operation_id,
        command: usagi_core::domain::supervisor::SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: run.supervisor_run_id,
            reason: "operator cancelled".into(),
        },
        observed_state_revision: run.state_revision,
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    super::spawn_work_run_control_job(Box::new(PanickingWorkRuns), workspace, request, sender);
    let super::WorkRunLaneCompletion::Control {
        operation_id: returned_operation,
        result,
        ..
    } = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a panicking control still returns its port")
    else {
        panic!("control job returned an observation completion");
    };
    assert_eq!(returned_operation, operation_id);
    let result = *result;
    assert_eq!(
        result.unwrap_err(),
        super::WorkRunControlError::Unconfirmed(super::WORK_RUN_ACTION_UNCONFIRMED.to_owned())
    );
}

/// One round asks each *other* open project for its own inventory. An
/// answer that names a different workspace is dropped rather than drawn in
/// the plot that was asked about, and the port comes back either way.
#[test]
fn a_garden_round_observes_every_other_project_and_drops_a_mismatched_answer() {
    struct FakeGardenInventory {
        answers: BTreeMap<WorkspaceId, Result<AgentWorkspaceObservation, String>>,
        asked: Arc<Mutex<Vec<WorkspaceId>>>,
    }

    impl super::GardenInventoryPort for FakeGardenInventory {
        fn inventory(
            &mut self,
            workspace: WorkspaceId,
        ) -> Result<AgentWorkspaceObservation, String> {
            self.asked.lock().unwrap().push(workspace);
            self.answers
                .remove(&workspace)
                .unwrap_or_else(|| Err("daemon unavailable".to_owned()))
        }
    }

    // An embedder that supplies no Garden lane keeps every other project's
    // plot read-only instead of failing the frame.
    assert!(
        UnavailableGardenInventoryPort
            .inventory(WorkspaceId::new())
            .is_err()
    );

    let observed = WorkspaceId::new();
    let mismatched = WorkspaceId::new();
    let unavailable = WorkspaceId::new();
    let empty_inventory = |workspace| AgentWorkspaceObservation {
        inventory: AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        },
        session_statuses: BTreeMap::new(),
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let port = FakeGardenInventory {
        answers: [
            (observed, Ok(empty_inventory(observed))),
            // The daemon answered about someone else: that is not evidence
            // about the project this round asked for.
            (mismatched, Ok(empty_inventory(WorkspaceId::new()))),
        ]
        .into_iter()
        .collect(),
        asked: Arc::clone(&asked),
    };
    let (sender, completions) = std::sync::mpsc::channel();
    // More projects than one round observes, so the bound is exercised too.
    let mut targets = vec![observed, mismatched, unavailable];
    targets.extend((0..super::MAX_OBSERVED_PROJECTS).map(|_| WorkspaceId::new()));

    super::spawn_garden_observation_job(Box::new(port), targets, sender);
    let completion = completions
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the round returns its port");

    assert_eq!(
        completion
            .inventories
            .iter()
            .map(|inventory| inventory.inventory.workspace_id)
            .collect::<Vec<_>>(),
        vec![observed]
    );
    assert_eq!(asked.lock().unwrap().len(), super::MAX_OBSERVED_PROJECTS);
}

#[test]
fn restore_retry_backoff_bounds_long_outage_and_reconnect_dispatches_once() {
    let mut retry = super::RestoreRetryState::new();
    let mut jobs = 0_u32;
    let mut rpc_attempts = 0_u32;
    let mut notices = 0_u32;
    let mut frames = 0_u32;
    let end = std::time::Duration::from_secs(60);
    let mut now = std::time::Duration::ZERO;
    while now <= end {
        frames += 1;
        if retry.begin_if_due(now) {
            jobs += 1;
            // One bounded worker attempts a terminal/Agent/terminal
            // consistency bracket three times; ticks add no RPCs.
            rpc_attempts += 9;
            notices += u32::from(retry.complete(now, super::RestoreJobOutcome::TransportFailed));
        }
        now += std::time::Duration::from_millis(16);
    }
    assert!(frames > 3_000, "the render/input clock stayed live");
    assert!(jobs <= 20, "capped backoff bounded worker churn: {jobs}");
    assert_eq!(rpc_attempts, jobs * 9);
    assert_eq!(notices, 1, "one outage produces one notice");

    retry.reconnected(1, end);
    retry.reconnected(1, end);
    assert!(retry.begin_if_due(end));
    assert!(
        !retry.begin_if_due(end),
        "only one restore can be in flight"
    );
    assert!(!retry.complete(end, super::RestoreJobOutcome::Applied));
    for offset in 1..=1_000 {
        assert!(!retry.begin_if_due(end + std::time::Duration::from_millis(offset)));
    }

    let mut outage = super::RestoreRetryState::new();
    assert!(outage.begin_if_due(std::time::Duration::ZERO));
    assert!(outage.complete(
        std::time::Duration::ZERO,
        super::RestoreJobOutcome::TransportFailed
    ));
    outage.request_observation(std::time::Duration::from_millis(10));
    assert!(
        !outage.begin_if_due(std::time::Duration::from_millis(10)),
        "a local Reopen cannot bypass the outage epoch backoff"
    );
    assert!(outage.begin_if_due(std::time::Duration::from_millis(250)));

    let mut in_flight = super::RestoreRetryState::new();
    assert!(in_flight.begin_if_due(std::time::Duration::ZERO));
    in_flight.request_observation(std::time::Duration::from_millis(1));
    assert!(!in_flight.complete(
        std::time::Duration::from_millis(1),
        super::RestoreJobOutcome::Applied
    ));
    assert!(!in_flight.begin_if_due(std::time::Duration::from_secs(1)));

    let mut changed_idle = super::RestoreRetryState::new();
    assert!(changed_idle.begin_if_due(std::time::Duration::ZERO));
    assert!(!changed_idle.complete(std::time::Duration::ZERO, super::RestoreJobOutcome::Applied));
    changed_idle.request_changed_observation(std::time::Duration::from_millis(1));
    assert!(changed_idle.begin_if_due(std::time::Duration::from_millis(1)));

    let mut changed_in_flight = super::RestoreRetryState::new();
    assert!(changed_in_flight.begin_if_due(std::time::Duration::ZERO));
    changed_in_flight.request_changed_observation(std::time::Duration::from_millis(1));
    assert!(!changed_in_flight.complete(
        std::time::Duration::from_millis(1),
        super::RestoreJobOutcome::Applied
    ));
    assert!(
        changed_in_flight.begin_if_due(std::time::Duration::from_millis(1)),
        "an in-flight snapshot may predate an Agent exit and needs one follow-up"
    );
    assert!(!changed_in_flight.begin_if_due(std::time::Duration::from_millis(1)));
}

#[test]
fn failed_restore_keeps_the_port_for_a_reconnect_dispatch() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    super::spawn_restore_job(
        Box::new(RetryRestorePort {
            workspace,
            entries: Vec::new(),
            runtimes: Vec::new(),
            fail_attempts: usize::MAX,
            terminal_attempts: Arc::new(AtomicUsize::new(0)),
            agent_attempts: Arc::new(AtomicUsize::new(0)),
        }),
        workspace,
        BTreeSet::from([session]),
        0,
        0,
        sender,
    );
    let completion = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

    let applied = super::apply_restore_completion(
        completion,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(applied.outcome, super::RestoreJobOutcome::TransportFailed);
    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(retry.complete(
        std::time::Duration::ZERO,
        super::RestoreJobOutcome::TransportFailed
    ));
    assert!(runtime.state().notice().is_none());
}

#[test]
#[allow(clippy::too_many_lines)] // Runtime, durable bytes, and retry fencing share one race fixture.
fn late_restore_leaves_runtime_and_durable_intent_bytes_unchanged() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: first,
        terminal: first_terminal.clone(),
        select: true,
    });
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: second,
        terminal: second_terminal.clone(),
        select: false,
    });
    initial.revision = 4;
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Down);
    let _ = runtime.handle_key(Key::Enter);
    let (dispatched_interaction, dispatched_revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        dispatched_interaction,
        dispatched_revision,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second_terminal.clone(),
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));

    // These are the user changes which make the dispatched observation
    // stale: reorder, select the survivor, then close the former selection.
    let _ = runtime.reorder_tab(TabDirection::Next);
    let _ = runtime.focus_terminal(Target::Session(session), second_terminal.clone());
    let _ = runtime.focus_terminal(Target::Session(session), first_terminal.clone());
    let _ = runtime.close_focused_pane();
    let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Reorder {
        session_id: Some(session),
        continuations: vec![second, first],
    });
    let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Select {
        session_id: Some(session),
        continuation: Some(second),
    });
    let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss {
        continuation: first,
    });
    let durable_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let revision_before = durable.lock().unwrap().revision;
    let runtime_before = runtime.active_pane().clone();
    let mutation_count = mutations.lock().unwrap().len();

    let runtime_item = |continuation, terminal: &TerminalRef| AgentRuntimeInventoryItem {
        runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
            .unwrap(),
        continuation,
        state: AgentRuntimeInventoryState::Live,
        resumed_from: None,
    };
    let terminal_inventory = || {
        vec![
            TerminalInventoryEntry {
                terminal: first_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
            TerminalInventoryEntry {
                terminal: second_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            },
        ]
    };
    let agent_inventory = || AgentInventory {
        workspace_id: workspace,
        runtimes: vec![
            runtime_item(first, &first_terminal),
            runtime_item(second, &second_terminal),
        ],
        resumable: Vec::new(),
    };
    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let completion = super::RestoreCompletion {
        port: Box::new(UnavailableAgentCommandPort),
        dispatched_interaction,
        dispatched_registry_revision: dispatched_revision,
        dispatched_allowed_sessions: BTreeSet::from([session]),
        terminals: Ok(terminal_inventory()),
        agents: Ok(agent_inventory()),
        observation_coherent: true,
    };
    let applied = super::apply_restore_completion(
        completion,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(applied.outcome, super::RestoreJobOutcome::FenceRejected);
    assert_eq!(runtime.active_pane(), &runtime_before);
    assert_eq!(mutations.lock().unwrap().len(), mutation_count);
    assert_eq!(durable.lock().unwrap().revision, revision_before);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        durable_before
    );

    // A fence rejection is a local UI race, not a daemon outage. Return
    // the dedicated port and admit one observation immediately under the
    // fresh fence, without a notice/backoff or duplicate in-flight job.
    let redispatch_at = std::time::Duration::from_secs(1);
    assert!(!retry.complete(redispatch_at, applied.outcome));
    assert!(retry.begin_if_due(redispatch_at));
    assert!(!retry.begin_if_due(redispatch_at));

    let fresh_fence = runtime.restore_fence();
    let fresh = super::apply_restore_completion(
        super::RestoreCompletion {
            port: applied.port,
            dispatched_interaction: fresh_fence.0,
            dispatched_registry_revision: fresh_fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(terminal_inventory()),
            agents: Ok(agent_inventory()),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(fresh.outcome, super::RestoreJobOutcome::Applied);
    assert!(!retry.complete(redispatch_at, fresh.outcome));
    assert_eq!(mutations.lock().unwrap().len(), mutation_count + 1);
    assert_eq!(runtime.focused_terminal(), Some(second_terminal.clone()));
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert!(!runtime.active_pane().tabs().iter().any(|tab| matches!(
        tab,
        PaneTab::Live(LivePane { terminal, kind: PaneKind::Agent })
            if terminal.fences(&first_terminal)
    )));
    assert!(runtime.active_pane().tabs().iter().any(|tab| matches!(
        tab,
        PaneTab::Live(LivePane { terminal, kind: PaneKind::Agent })
            if terminal.fences(&second_terminal)
    )));
    assert!(!retry.begin_if_due(redispatch_at + std::time::Duration::from_secs(60)));
}

#[test]
#[allow(clippy::too_many_lines)] // The stale and fresh observations must share one durable fixture.
fn cross_tui_stale_observe_omits_old_ref_then_fresh_observation_restores_replacement() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let old = scoped_terminal_ref(workspace, Some(session));
    let replacement = scoped_terminal_ref(workspace, Some(session));
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: old.clone(),
        select: true,
    });
    initial.revision = 1;
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let dispatched = runtime.restore_fence();

    // Another TUI replaces O with R after this controller loaded revision 1.
    {
        let mut latest = durable.lock().unwrap();
        latest.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: replacement.clone(),
            select: true,
        });
        latest.revision += 1;
    }
    let inventory = |terminal: &TerminalRef| AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    let terminals = |terminal: &TerminalRef| {
        vec![TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        }]
    };
    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let stale = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: dispatched.0,
            dispatched_registry_revision: dispatched.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(terminals(&old)),
            agents: Ok(inventory(&old)),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(stale.outcome, super::RestoreJobOutcome::FenceRejected);
    assert!(runtime.active_pane().tabs().is_empty());
    assert_ne!(runtime.focused_terminal(), Some(old));
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&replacement)
    );
    let redispatch_at = std::time::Duration::from_secs(1);
    assert!(!retry.complete(redispatch_at, stale.outcome));
    assert!(retry.begin_if_due(redispatch_at));
    assert!(!retry.begin_if_due(redispatch_at));

    let fresh_fence = runtime.restore_fence();
    let fresh = super::apply_restore_completion(
        super::RestoreCompletion {
            port: stale.port,
            dispatched_interaction: fresh_fence.0,
            dispatched_registry_revision: fresh_fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(terminals(&replacement)),
            agents: Ok(inventory(&replacement)),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(fresh.outcome, super::RestoreJobOutcome::Applied);
    assert!(!retry.complete(redispatch_at, fresh.outcome));
    assert_eq!(runtime.focused_terminal(), Some(replacement));
    assert_eq!(mutations.lock().unwrap().len(), 2);
}

#[test]
#[allow(clippy::too_many_lines)] // This regression keeps the visible stale ref and latest lineage together.
fn visible_old_ref_can_close_latest_lineage_while_fresh_observation_is_pending() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let old = scoped_terminal_ref(workspace, Some(session));
    let replacement = scoped_terminal_ref(workspace, Some(session));
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: old.clone(),
        select: true,
    });
    initial.revision = 1;
    let durable = Arc::new(Mutex::new(initial));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let inventory = |terminal: &TerminalRef| AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    let terminals = |terminal: &TerminalRef| {
        vec![TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        }]
    };
    let completion =
        |terminal: &TerminalRef, fence: (u64, u64), port: Box<dyn AgentCommandPort>| {
            super::RestoreCompletion {
                port,
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::from([session]),
                terminals: Ok(terminals(terminal)),
                agents: Ok(inventory(terminal)),
                observation_coherent: true,
            }
        };

    let first_fence = runtime.restore_fence();
    let first = super::apply_restore_completion(
        completion(&old, first_fence, Box::new(UnavailableAgentCommandPort)),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(first.outcome, super::RestoreJobOutcome::Applied);
    assert_eq!(runtime.focused_terminal(), Some(old.clone()));

    // Another TUI advances this continuation from O to R. The late O
    // observation updates local durable state but must leave the visible O
    // pane untouched until its immediately scheduled fresh observation.
    {
        let mut latest = durable.lock().unwrap();
        latest.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: replacement.clone(),
            select: true,
        });
        latest.revision += 1;
    }
    let stale_fence = runtime.restore_fence();
    let stale = super::apply_restore_completion(
        completion(&old, stale_fence, first.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(stale.outcome, super::RestoreJobOutcome::FenceRejected);
    assert_eq!(runtime.focused_terminal(), Some(old.clone()));
    assert_eq!(ui.agent_continuation_for(&old), Some(continuation));

    super::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.focused_terminal(), Some(old.clone()));
    assert!(durable.lock().unwrap().dismissed.is_empty());
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&replacement)
    );

    let fresh_fence = runtime.restore_fence();
    let fresh = super::apply_restore_completion(
        completion(&replacement, fresh_fence, stale.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(fresh.outcome, super::RestoreJobOutcome::Applied);
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(runtime.focused_terminal(), Some(replacement));
}

#[test]
#[allow(clippy::too_many_lines)]
fn successful_restore_retains_port_and_reconnect_reobserves_exactly_once() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let terminal = scoped_terminal_ref(workspace, Some(session));
    let terminal_attempts = Arc::new(AtomicUsize::new(0));
    let agent_attempts = Arc::new(AtomicUsize::new(0));
    let port: Box<dyn AgentCommandPort> = Box::new(RetryRestorePort {
        workspace,
        entries: vec![TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        }],
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        fail_attempts: 0,
        terminal_attempts: Arc::clone(&terminal_attempts),
        agent_attempts: Arc::clone(&agent_attempts),
    });
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut retry = super::RestoreRetryState::new();

    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let fence = runtime.restore_fence();
    super::spawn_restore_job(
        port,
        workspace,
        BTreeSet::from([session]),
        fence.0,
        fence.1,
        sender.clone(),
    );
    let first = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let first = super::apply_restore_completion(
        first,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(first.outcome, super::RestoreJobOutcome::Applied);
    assert!(!retry.complete(std::time::Duration::ZERO, super::RestoreJobOutcome::Applied));
    assert_eq!(mutations.lock().unwrap().len(), 1);
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    let focus_before = runtime.focused_terminal();
    for tick in 1..=1_000 {
        assert!(!retry.begin_if_due(std::time::Duration::from_millis(tick)));
    }

    // A typed reconnect epoch, not a frame tick, admits one new observation
    // with the same dedicated port. RetryRestorePort::launch panics, so this
    // also proves reconnect inventory never becomes a spawn replay.
    let reconnect_at = std::time::Duration::from_secs(2);
    retry.reconnected(1, reconnect_at);
    retry.reconnected(1, reconnect_at);
    assert!(retry.begin_if_due(reconnect_at));
    assert!(!retry.begin_if_due(reconnect_at));
    let fence = runtime.restore_fence();
    super::spawn_restore_job(
        first.port,
        workspace,
        BTreeSet::from([session]),
        fence.0,
        fence.1,
        sender,
    );
    let second = receiver
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let second = super::apply_restore_completion(
        second,
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(second.outcome, super::RestoreJobOutcome::Applied);
    assert!(!retry.complete(reconnect_at, super::RestoreJobOutcome::Applied));
    assert_eq!(mutations.lock().unwrap().len(), 2);
    assert_eq!(runtime.focused_terminal(), focus_before);
    assert_eq!(terminal_attempts.load(Ordering::SeqCst), 4);
    assert_eq!(agent_attempts.load(Ordering::SeqCst), 2);
    assert!(!retry.begin_if_due(reconnect_at + std::time::Duration::from_secs(60)));
}

#[test]
#[allow(clippy::too_many_lines)] // Lifecycle cleanup, durable state, and retry admission share one fixture.
fn session_membership_change_requests_one_observation_and_cleans_owned_intent() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let removed_session = SessionId::new();
    let root_open = AgentContinuationRef::new();
    let root_dismissed = AgentContinuationRef::new();
    let removed_selected = AgentContinuationRef::new();
    let removed_dismissed = AgentContinuationRef::new();
    let root_open_terminal = scoped_terminal_ref(workspace, Some(session));
    let root_dismissed_terminal = scoped_terminal_ref(workspace, Some(session));
    let removed_selected_terminal = scoped_terminal_ref(workspace, Some(removed_session));
    let removed_dismissed_terminal = scoped_terminal_ref(workspace, Some(removed_session));
    let mut initial = AgentTabIntent::empty(workspace);
    for (session_id, continuation, terminal, select) in [
        (Some(session), root_open, root_open_terminal.clone(), true),
        (
            Some(session),
            root_dismissed,
            root_dismissed_terminal.clone(),
            false,
        ),
        (
            Some(removed_session),
            removed_selected,
            removed_selected_terminal.clone(),
            true,
        ),
        (
            Some(removed_session),
            removed_dismissed,
            removed_dismissed_terminal.clone(),
            false,
        ),
    ] {
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id,
            continuation,
            terminal,
            select,
        });
    }
    initial.apply(AgentTabIntentMutation::Dismiss {
        continuation: root_dismissed,
    });
    initial.apply(AgentTabIntentMutation::Dismiss {
        continuation: removed_dismissed,
    });
    initial.revision = 9;
    initial.validate(workspace).unwrap();
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session, removed_session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    let initial_fence = runtime.restore_fence();
    let initial_pairs = [
        (root_open_terminal.clone(), root_open),
        (root_dismissed_terminal, root_dismissed),
        (removed_selected_terminal, removed_selected),
        (removed_dismissed_terminal, removed_dismissed),
    ];
    let initial_restore = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: initial_fence.0,
            dispatched_registry_revision: initial_fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session, removed_session]),
            terminals: Ok(initial_pairs
                .iter()
                .map(|(terminal, _)| TerminalInventoryEntry {
                    terminal: terminal.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                })
                .collect()),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: initial_pairs
                    .iter()
                    .map(|(terminal, continuation)| AgentRuntimeInventoryItem {
                        runtime: AgentRuntimeRef::new(
                            AgentRuntimeId::new(),
                            terminal.clone(),
                            terminal.session_id,
                        )
                        .unwrap(),
                        continuation: *continuation,
                        state: AgentRuntimeInventoryState::Live,
                        resumed_from: None,
                    })
                    .collect(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session, removed_session]),
    );
    assert_eq!(initial_restore.outcome, super::RestoreJobOutcome::Applied);
    assert!(!retry.complete(std::time::Duration::ZERO, initial_restore.outcome));
    assert_eq!(mutations.lock().unwrap().len(), 1);
    assert!(!ui.take_agent_observation_request());

    ui.set_allowed_agent_sessions(BTreeSet::from([session]));
    assert!(ui.take_agent_observation_request());
    ui.set_allowed_agent_sessions(BTreeSet::from([session]));
    assert!(!ui.take_agent_observation_request());
    let now = std::time::Duration::from_secs(1);
    retry.request_observation(now);
    assert!(retry.begin_if_due(now));
    assert!(!retry.begin_if_due(now));
    let fence = runtime.restore_fence();
    let applied = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: root_open_terminal.clone(),
                kind: TerminalKind::Agent,
                live: true,
            }]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: vec![AgentRuntimeInventoryItem {
                    runtime: AgentRuntimeRef::new(
                        AgentRuntimeId::new(),
                        root_open_terminal.clone(),
                        Some(session),
                    )
                    .unwrap(),
                    continuation: root_open,
                    state: AgentRuntimeInventoryState::Live,
                    resumed_from: None,
                }],
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );

    assert_eq!(applied.outcome, super::RestoreJobOutcome::Applied);
    assert!(!retry.complete(now, applied.outcome));
    let mutations = mutations.lock().unwrap();
    assert_eq!(mutations.len(), 2);
    assert!(matches!(
        mutations.as_slice(),
        [
            AgentTabIntentMutation::Observe {
                allowed_sessions: initial_allowed,
                ..
            },
            AgentTabIntentMutation::Observe {
                allowed_sessions: removed_allowed,
                ..
            }
        ] if *initial_allowed == BTreeSet::from([session, removed_session])
            && *removed_allowed == BTreeSet::from([session])
    ));
    drop(mutations);
    let durable = durable.lock().unwrap();
    durable.validate(workspace).unwrap();
    assert!(
        durable
            .targets
            .iter()
            .all(|target| target.session_id != Some(removed_session))
    );
    assert_eq!(durable.dismissed, BTreeSet::from([root_dismissed]));
    assert!(
        durable.targets[0]
            .tabs
            .iter()
            .any(|slot| slot.continuation == root_open)
    );
    assert!(!durable.dismissed.contains(&removed_dismissed));
    assert_eq!(runtime.focused_terminal(), Some(root_open_terminal));
    assert!(!retry.begin_if_due(now + std::time::Duration::from_secs(60)));
}

#[test]
#[allow(clippy::too_many_lines)] // One target matrix fixes Agent ordering and generic deduplication.
fn reconciled_agent_order_precedes_deterministic_generic_inventory_per_target() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let stale_session = SessionId::new();
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let first_terminal = scoped_terminal_ref(workspace, None);
    let second_terminal = scoped_terminal_ref(workspace, None);
    let session_agent = scoped_terminal_ref(workspace, Some(session));
    let root_generic = scoped_terminal_ref(workspace, None);
    let session_generic = scoped_terminal_ref(workspace, Some(session));
    let session_generic_second = scoped_terminal_ref(workspace, Some(session));
    let stale_generic = scoped_terminal_ref(workspace, Some(stale_session));
    let projection = AgentTabProjection {
        targets: vec![
            AgentTabTargetProjection {
                session_id: None,
                tabs: vec![
                    AgentTabSlotIntent {
                        continuation: second,
                        terminal: second_terminal.clone(),
                    },
                    AgentTabSlotIntent {
                        continuation: first,
                        terminal: first_terminal.clone(),
                    },
                ],
                selected: Some(first),
            },
            AgentTabTargetProjection {
                session_id: Some(session),
                tabs: vec![AgentTabSlotIntent {
                    continuation: AgentContinuationRef::new(),
                    terminal: session_agent.clone(),
                }],
                selected: None,
            },
        ],
    };
    let entries = [
        TerminalInventoryEntry {
            terminal: session_generic.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: root_generic.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_generic_second.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_generic.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: stale_generic,
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: first_terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: first_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
    ];

    let targets = super::pane_restore_targets(
        workspace,
        &BTreeSet::from([session]),
        projection,
        &entries,
        Some(&session_generic_second),
        Vec::new(),
        &BTreeMap::new(),
    );
    assert_eq!(targets.len(), 2);
    let root = targets
        .iter()
        .find(|target| target.target == Target::Root(workspace))
        .unwrap();
    assert_eq!(root.selected, Some(first_terminal));
    assert_eq!(root.panes[0].terminal, second_terminal);
    assert_eq!(root.panes[1].kind, PaneKind::Agent);
    assert_eq!(root.panes.len(), 3);
    assert!(
        root.panes
            .iter()
            .any(|pane| pane.kind == PaneKind::Terminal && pane.terminal.fences(&root_generic))
    );
    let managed = targets
        .iter()
        .find(|target| target.target == Target::Session(session))
        .unwrap();
    assert_eq!(managed.selected, Some(session_generic_second.clone()));
    assert_eq!(managed.panes[0].terminal, session_agent);
    assert!(
        managed
            .panes
            .iter()
            .any(|pane| pane.terminal.fences(&session_generic))
    );
    assert!(
        managed
            .panes
            .iter()
            .any(|pane| pane.terminal.fences(&session_generic_second))
    );
    assert_eq!(
        managed
            .panes
            .iter()
            .filter(|pane| pane.terminal.fences(&session_generic))
            .count(),
        1
    );
}

#[test]
fn coherent_empty_projection_authoritatively_clears_every_scoped_live_target() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let stale = scoped_terminal_ref(workspace, Some(session));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![LivePane {
                terminal: stale,
                kind: PaneKind::Agent,
            }],
            selected: None,
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    assert_eq!(
        runtime
            .panes()
            .pane(Target::Session(session))
            .unwrap()
            .tabs()
            .len(),
        1
    );

    let empty = super::pane_restore_targets(
        workspace,
        &BTreeSet::from([session]),
        AgentTabProjection::default(),
        &[],
        None,
        Vec::new(),
        &BTreeMap::new(),
    );
    assert_eq!(empty.len(), 2);
    assert!(empty.iter().all(|target| target.panes.is_empty()));
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(fence.0, fence.1, empty));
    assert!(
        runtime
            .panes()
            .pane(Target::Session(session))
            .unwrap()
            .tabs()
            .is_empty()
    );
}

#[test]
fn foreground_sync_attaches_only_the_active_selected_tab() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = scoped_terminal_ref(workspace, Some(session));
    let second = scoped_terminal_ref(workspace, Some(session));
    let detaches = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(ScriptedAgentPort {
                terminal: first.clone(),
                subscription: 41,
                replay: b"retained".to_vec(),
                poll_error: None,
                detaches: Arc::clone(&detaches),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let (interaction, revision) = runtime.restore_fence();
    let _ = runtime.restore_snapshot(
        interaction,
        revision,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second.clone(),
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    );
    let geometry = terminal_geometry(20, 80);

    ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
    // Re-syncing while the same selection is already attached keeps it in
    // place, exercising the fence check that avoids relaunching a live
    // foreground terminal.
    ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
    assert!(ui.terminal_rows(&first, None).is_some());
    assert!(ui.terminal_rows(&second, None).is_none());

    let _ = runtime.focus_terminal(Target::Session(session), second.clone());
    ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
    assert!(ui.terminal_rows(&first, None).is_none());
    assert!(ui.terminal_rows(&second, None).is_some());
    assert_eq!(*detaches.lock().unwrap(), vec![41]);
}

#[test]
#[allow(clippy::too_many_lines)] // One stream fixture proves simultaneous attachment, polling, and stable geometry.
fn drawers_keep_the_managed_terminal_at_home_geometry_and_moving() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = scoped_terminal_ref(workspace, Some(session));
    let root_agent = scoped_terminal_ref(workspace, None);
    let root_shell = scoped_terminal_ref(workspace, None);
    let initial = b"one\r\ntwo\r\nthree";
    let moved = b"\r\ndim-managed-moved";
    let calls = Arc::new(Mutex::new(StreamCalls {
        scripted_polls: vec![(
            managed.clone(),
            vec![TerminalChunk {
                start_offset: initial.len() as u64,
                end_offset: (initial.len() + moved.len()) as u64,
                data: moved.to_vec(),
            }],
        )],
        ..StreamCalls::default()
    }));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RecordingStreamPort(Arc::clone(&calls))),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![
            super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![
                    LivePane {
                        terminal: root_agent.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: root_shell.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(root_agent.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
            super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: managed.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(managed.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));
    let managed_geometry = terminal_geometry(24, 100);
    let managed_background_geometry = super::managed_background_terminal_geometry(24, 100);
    assert_eq!(managed_background_geometry, managed_geometry);
    let director_geometry = foreground_terminal_geometry(
        24,
        100,
        true,
        false,
        false,
        Some(WorkspaceDrawerFocus::Director),
    );

    ui.sync_foreground_terminal(Some(&managed), managed_geometry);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_drawer_background_attachment(&runtime, &managed);
    ui.sync_visible_terminals(&[
        (root_agent.clone(), director_geometry),
        (managed.clone(), managed_background_geometry),
    ]);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(runtime.focused_terminal(), Some(root_shell.clone()));
    let attachments = super::workspace_terminal_attachments(&runtime, 24, 100);
    assert!(attachments.iter().any(|(terminal, geometry)| {
        terminal.fences(&managed) && *geometry == managed_geometry
    }));
    let root_shell_geometry = attachments
        .iter()
        .find_map(|(terminal, geometry)| terminal.fences(&root_shell).then_some(*geometry))
        .expect("the root shell owns the terminal drawer");
    ui.sync_visible_terminals(&attachments);
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminalFullHeight));
    assert!(runtime.state().root_terminal_full_height());
    let full_height_attachments = super::workspace_terminal_attachments(&runtime, 24, 100);
    assert!(full_height_attachments.iter().any(|(terminal, geometry)| {
        terminal.fences(&managed) && *geometry == managed_geometry
    }));
    let full_height_root_geometry = full_height_attachments
        .iter()
        .find_map(|(terminal, geometry)| terminal.fences(&root_shell).then_some(*geometry))
        .expect("the full-height drawer keeps the root shell attached");
    assert_ne!(full_height_root_geometry, root_shell_geometry);
    ui.sync_visible_terminals(&full_height_attachments);
    close_exited_panes(&mut ui, &mut runtime);

    let dimmed_view = ui
        .retained_terminal_view(&managed, 2)
        .expect("the visible managed terminal remains projected");
    assert!(
        strip_ansi(&dimmed_view.rows.join("\n")).contains("dim-managed-moved"),
        "the attached background stream must advance its retained view"
    );
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.attach_geometries,
        [
            (managed.clone(), managed_geometry),
            (root_agent.clone(), director_geometry),
            (root_shell.clone(), root_shell_geometry),
        ]
    );
    assert_eq!(
        calls.resize_geometries,
        [(root_shell.clone(), full_height_root_geometry)],
        "only the terminal inside a resized drawer may change geometry"
    );
    assert!(
        calls
            .poll_terminals
            .iter()
            .any(|terminal| terminal == &root_agent)
    );
    assert!(
        calls
            .poll_terminals
            .iter()
            .any(|terminal| terminal == &root_shell)
    );
    assert!(
        calls
            .poll_terminals
            .iter()
            .any(|terminal| terminal == &managed)
    );
    assert_eq!(
        calls.background_watches.last().cloned(),
        Some(Vec::new()),
        "the attached dimmed terminal must not also enter inventory polling"
    );
    assert_eq!(calls.detaches, 0);
}

fn assert_drawer_background_attachment(runtime: &WorkspaceRuntime, managed: &TerminalRef) {
    for width in [80, 79] {
        let attached = super::workspace_terminal_attachments(runtime, 24, width);
        assert!(
            attached.iter().any(|(terminal, geometry)| {
                terminal.fences(managed) && *geometry == terminal_geometry(24, width)
            }),
            "a drawer, including its full-width form, must retain the background at Home geometry"
        );
    }
}

#[test]
fn concurrent_drawers_keep_root_surfaces_and_selected_session_agent_visible() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = scoped_terminal_ref(workspace, Some(session));
    let root_agent = scoped_terminal_ref(workspace, None);
    let root_terminal = scoped_terminal_ref(workspace, None);
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![
            super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: managed.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(managed.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
            super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![
                    LivePane {
                        terminal: root_agent.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: root_terminal.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(root_agent.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert!(runtime.state().director_drawer_open());
    assert!(runtime.state().root_terminal_drawer_open());
    assert_eq!(runtime.focused_terminal(), Some(root_terminal.clone()));
    let visible = super::workspace_terminal_attachments(&runtime, 30, 160);
    assert_eq!(
        visible
            .iter()
            .map(|(terminal, _)| terminal)
            .collect::<Vec<_>>(),
        [&root_terminal, &managed, &root_agent]
    );
    assert_eq!(
        visible[1].1,
        terminal_geometry(30, 160),
        "drawers must not shrink the background workspace geometry"
    );
    assert_eq!(
        visible[0].1,
        Geometry { cols: 60, rows: 10 },
        "the root Shell PTY must fit the band left of Director"
    );
    let fully_occluded = super::workspace_terminal_attachments(&runtime, 8, 160);
    assert!(
        fully_occluded
            .iter()
            .any(|(terminal, _)| terminal.fences(&managed)),
        "a full-height root overlay must retain its background Agent"
    );
    assert_eq!(workspace::root_terminal_available_width(30, 160, true), 64);
    assert_eq!(workspace::root_terminal_available_width(30, 79, true), 79);

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(root_agent.clone()));
    let visible = super::workspace_terminal_attachments(&runtime, 30, 160)
        .into_iter()
        .map(|(terminal, _)| terminal)
        .collect::<Vec<_>>();
    assert_eq!(visible, [root_agent, managed, root_terminal]);
}

fn sync_test_director_terminals(
    ui: &mut WorkspaceIoRuntime,
    root: &TerminalRef,
    managed: &TerminalRef,
    height: usize,
    width: usize,
) {
    ui.sync_visible_terminals(&[
        (
            root.clone(),
            foreground_terminal_geometry(
                height,
                width,
                true,
                false,
                false,
                Some(WorkspaceDrawerFocus::Director),
            ),
        ),
        (
            managed.clone(),
            super::managed_background_terminal_geometry(height, width),
        ),
    ]);
}

fn plain_terminal_rows(view: &super::TerminalViewProjection) -> String {
    strip_ansi(&view.rows.join("\n"))
        .replace(TERMINAL_CURSOR_MARKER, "")
        .trim_end()
        .to_owned()
}

#[test]
#[allow(clippy::too_many_lines)] // One round trip retains both surfaces' independent viewport and selection state.
fn drawer_round_trip_restores_both_views_and_restates_each_viewport_without_resync() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = scoped_terminal_ref(workspace, Some(session));
    let root = scoped_terminal_ref(workspace, None);
    let calls = Arc::new(Mutex::new(StreamCalls::default()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RecordingStreamPort(Arc::clone(&calls))),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        interaction,
        revision,
        vec![
            super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![LivePane {
                    terminal: root.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(root.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
            super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: managed.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(managed.clone()),
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));
    let managed_geometry = terminal_geometry(24, 100);
    let drawer_geometry = foreground_terminal_geometry(
        24,
        100,
        true,
        false,
        false,
        Some(WorkspaceDrawerFocus::Director),
    );
    let mut controls = LiveTerminalControls::default();

    ui.sync_foreground_terminal(Some(&managed), managed_geometry);
    let _ = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    controls.scroll_up();
    controls.begin_selection(TerminalSelection::begin(
        vec!["managed".to_owned()],
        TerminalPoint { row: 0, column: 0 },
    ));
    controls.extend_selection(TerminalPoint { row: 0, column: 6 });
    let _ = controls.finish_drag();

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(root.clone()));
    sync_test_director_terminals(&mut ui, &root, &managed, 24, 100);
    let retained = ui.retained_terminal_view(&managed, 1).unwrap();
    assert_eq!(plain_terminal_rows(&retained), "three");
    let _ = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    controls.scroll_up();
    controls.scroll_up();
    controls.begin_selection(TerminalSelection::begin(
        vec!["drawer".to_owned()],
        TerminalPoint { row: 0, column: 0 },
    ));
    controls.extend_selection(TerminalPoint { row: 0, column: 5 });
    let _ = controls.finish_drag();

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(managed.clone()));
    ui.sync_foreground_terminal(Some(&managed), managed_geometry);
    let managed_view = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    assert_eq!(managed_view.scroll, 1);
    assert_eq!(
        controls.selection().map(TerminalSelection::text).as_deref(),
        Some("managed")
    );

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert_eq!(runtime.focused_terminal(), Some(root.clone()));
    sync_test_director_terminals(&mut ui, &root, &managed, 24, 100);
    let drawer_view = controller_terminal_view(&ui, &runtime, &mut controls, 1).unwrap();
    assert_eq!(drawer_view.scroll, 2);
    assert_eq!(
        controls.selection().map(TerminalSelection::text).as_deref(),
        Some("drawer")
    );

    let calls = calls.lock().unwrap();
    // The managed pane stays attached at its normal workspace geometry
    // through both Director visits. Only the root conversation is detached
    // when the overlay closes and attached again when it reopens.
    assert_eq!(
        calls.attach_geometries,
        [
            (managed.clone(), managed_geometry),
            (root.clone(), drawer_geometry),
            (root, drawer_geometry),
        ]
    );
    assert!(
        calls.resize_geometries.is_empty(),
        "drawer round trips must leave the managed terminal geometry untouched"
    );
    assert_eq!(calls.attaches, 3);
    assert_eq!(calls.detaches, 1);
}

#[test]
fn detached_terminal_coordinators_are_bounded_and_evict_the_oldest() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminals = (0..=super::DETACHED_TERMINAL_LIMIT)
        .map(|_| scoped_terminal_ref(workspace, Some(session)))
        .collect::<Vec<_>>();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        );
    let geometry = terminal_geometry(20, 80);

    // Embedders can lose their stream port before teardown. Closing the
    // retained coordinator still removes and retains it without a detach.
    let without_agent = scoped_terminal_ref(workspace, Some(session));
    let mut embedded = WorkspaceIoRuntime::new(
        WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]),
        Box::new(UnavailableSessionCommandPort),
    );
    embedded.terminals.push(
        crate::usecase::application::terminal_session::TerminalSession::new(
            without_agent.clone(),
            geometry,
        ),
    );
    embedded.close_terminal(&without_agent);
    assert_eq!(embedded.detached_terminals.len(), 1);

    for terminal in &terminals {
        ui.start_terminal_session(terminal.clone(), geometry);
        ui.close_terminal(terminal);
    }

    assert_eq!(ui.detached_terminals.len(), super::DETACHED_TERMINAL_LIMIT);
    assert!(
        !ui.detached_terminals
            .iter()
            .any(|retained| retained.terminal().fences(&terminals[0]))
    );
    assert!(
        ui.detached_terminals
            .iter()
            .any(|retained| retained.terminal().fences(terminals.last().unwrap()))
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Close and reopen rollback use the same failure fixture.
fn persistence_failures_leave_close_and_reopen_ui_unchanged_with_typed_notice() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let continuation = history.continuation;
    let terminal = history.last_terminal.clone();
    let open_intent = AgentTabIntent::empty(workspace);
    let durable = Arc::new(Mutex::new(open_intent));
    let attempts = Arc::new(AtomicUsize::new(0));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&durable),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: None,
            interrupted: vec![history],
        }],
    ));
    let _ = runtime.select_tab(TabDirection::Next);

    super::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert!(runtime.focused_terminal().is_none());
    assert_eq!(runtime.active_pane().tabs().len(), 1);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );

    let mut closed_intent = AgentTabIntent::empty(workspace);
    closed_intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal,
        select: true,
    });
    closed_intent.apply(AgentTabIntentMutation::Dismiss { continuation });
    let closed = Arc::new(Mutex::new(closed_intent));
    let closed_bytes = serde_json::to_vec(&*closed.lock().unwrap()).unwrap();
    let reopen_attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&closed),
                error: AgentTabIntentError::ReadOnlySchema,
                attempts: Arc::clone(&reopen_attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::ReopenAgent(ReopenAgentRequest {
            workspace,
            continuation,
        }))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert!(runtime.active_pane().tabs().is_empty());
    assert_eq!(reopen_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_vec(&*closed.lock().unwrap()).unwrap(),
        closed_bytes
    );
    assert!(closed.lock().unwrap().dismissed.contains(&continuation));
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::ReadOnlySchema.safe_message())
    );
}

#[test]
#[allow(clippy::too_many_lines)] // The stale-cache regression needs both pane kinds and a fresh observation.
fn same_tui_reopen_waits_for_fresh_observation_and_preserves_new_generic_pane() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let continuation = AgentContinuationRef::new();
    let agent_terminal = scoped_terminal_ref(workspace, Some(session));
    let generic_terminal = scoped_terminal_ref(workspace, Some(session));
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::clone(&mutations),
            }),
        );
    // Establish the old empty observation, then admit both panes later in
    // this TUI. Reopen must never rebuild from that obsolete snapshot.
    assert!(
        ui.observe_agent_tabs(
            Vec::new(),
            AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            },
        )
        .unwrap()
        .cas_accepted
    );
    ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: agent_terminal.clone(),
        select: true,
    })
    .unwrap();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: agent_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: generic_terminal.clone(),
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(agent_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    super::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.focused_terminal(), Some(agent_terminal.clone()));
    assert!(durable.lock().unwrap().dismissed.is_empty());

    // Seed legacy hidden state to exercise compatibility with an older
    // writer. The current UI itself never creates this state.
    ui.mutate_agent_intent(AgentTabIntentMutation::Dismiss { continuation })
        .unwrap();
    let _ = runtime.close_focused_pane();
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal.clone()));

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::ReopenAgent(ReopenAgentRequest {
            workspace,
            continuation,
        }))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert!(!durable.lock().unwrap().dismissed.contains(&continuation));
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal.clone()));
    assert!(matches!(
        runtime.active_pane().tabs(),
        [PaneTab::Live(LivePane { terminal, kind: PaneKind::Terminal })]
            if terminal.fences(&generic_terminal)
    ));
    assert!(ui.take_agent_observation_request());

    let now = std::time::Duration::from_secs(1);
    let mut retry = super::RestoreRetryState::new();
    assert!(retry.begin_if_due(std::time::Duration::ZERO));
    assert!(!retry.complete(std::time::Duration::ZERO, super::RestoreJobOutcome::Applied));
    retry.request_observation(now);
    assert!(retry.begin_if_due(now));
    assert!(!retry.begin_if_due(now));
    let fence = runtime.restore_fence();
    let applied = super::apply_restore_completion(
        super::RestoreCompletion {
            port: Box::new(UnavailableAgentCommandPort),
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![
                TerminalInventoryEntry {
                    terminal: agent_terminal.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                },
                TerminalInventoryEntry {
                    terminal: generic_terminal.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                },
            ]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: vec![AgentRuntimeInventoryItem {
                    runtime: AgentRuntimeRef::new(
                        AgentRuntimeId::new(),
                        agent_terminal.clone(),
                        Some(session),
                    )
                    .unwrap(),
                    continuation,
                    state: AgentRuntimeInventoryState::Live,
                    resumed_from: None,
                }],
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        },
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(applied.outcome, super::RestoreJobOutcome::Applied);
    assert!(!retry.complete(now, applied.outcome));
    let restored = runtime
        .active_pane()
        .tabs()
        .iter()
        .filter_map(|tab| match tab {
            PaneTab::Live(pane) => Some(pane.terminal.clone()),
            PaneTab::Pending(_) | PaneTab::Ready(_) | PaneTab::Interrupted(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(restored, vec![agent_terminal, generic_terminal.clone()]);
    assert_eq!(runtime.focused_terminal(), Some(generic_terminal));
    assert_eq!(
        mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|mutation| matches!(mutation, AgentTabIntentMutation::Observe { .. }))
            .count(),
        2
    );
}

#[test]
fn stale_agent_admission_cannot_show_or_focus_a_lineage_closed_by_another_tui() {
    let workspace = WorkspaceId::new();
    let continuation = AgentContinuationRef::new();
    let original = scoped_terminal_ref(workspace, None);
    let replacement = scoped_terminal_ref(workspace, None);
    let mut initial = AgentTabIntent::empty(workspace);
    initial.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation,
        terminal: original.clone(),
        select: true,
    });
    initial.revision = 1;
    let durable = Arc::new(Mutex::new(initial));
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::new(),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations,
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let operation = OperationId::new();
    let target = Target::Root(workspace);
    let _ = runtime.request_pane(target, operation, PaneKind::Agent);
    let mut pending = std::collections::HashMap::from([(operation, target)]);

    // A second writer closes this continuation after the first TUI loaded
    // revision 1 but before its daemon admission returns.
    {
        let mut latest = durable.lock().unwrap();
        let _ = latest.apply(AgentTabIntentMutation::Dismiss { continuation });
        latest.revision += 1;
    }
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Agent {
                operation,
                result: Ok(AgentPaneAdmission {
                    terminal: replacement.clone(),
                    continuation: Some(continuation),
                    supervisor_run_id: None,
                }),
            },
        })
        .unwrap();

    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    assert!(runtime.active_pane().tabs().is_empty());
    assert_eq!(runtime.focused_terminal(), None);
    assert!(durable.lock().unwrap().dismissed.contains(&continuation));
    assert!(
        durable.lock().unwrap().targets[0].tabs[0]
            .terminal
            .fences(&original)
    );
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::ConcurrentChange.safe_message())
    );
}

#[test]
fn closing_selected_agent_keeps_it_visible_without_focus_drift() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = AgentContinuationRef::new();
    let closed = AgentContinuationRef::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let closed_terminal = scoped_terminal_ref(workspace, Some(session));
    let generic = scoped_terminal_ref(workspace, Some(session));
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: first,
        terminal: first_terminal.clone(),
        select: false,
    });
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation: closed,
        terminal: closed_terminal.clone(),
        select: true,
    });
    let durable = Arc::new(Mutex::new(intent));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(WheelRecordingPort {
                terminal: generic.clone(),
                replay: Vec::new(),
                inputs: Arc::new(Mutex::new(Vec::new())),
                input_error: false,
            }),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first_terminal,
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: closed_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: generic.clone(),
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(generic.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let _ = runtime.focus_terminal(
        Target::Session(session),
        durable.lock().unwrap().targets[0].tabs[1].terminal.clone(),
    );

    super::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert_eq!(runtime.focused_terminal(), Some(closed_terminal.clone()));
    {
        let state = durable.lock().unwrap();
        assert!(state.dismissed.is_empty());
        assert_eq!(state.targets[0].selected, Some(closed));
    }

    // Closing a generic tab is not a durable conversation dismissal. It
    // records only a process-local exact fence against inventory restore.
    let _ = runtime.focus_terminal(Target::Session(session), generic.clone());
    ui.start_terminal_session(generic.clone(), terminal_geometry(20, 80));
    let before = durable.lock().unwrap().clone();
    super::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(!runtime.active_pane().tabs().iter().any(|tab| matches!(
        tab,
        PaneTab::Live(LivePane { terminal, .. }) if terminal.fences(&generic)
    )));
    assert_eq!(*durable.lock().unwrap(), before);
    assert_eq!(ui.closed_generic_terminals, BTreeSet::from([generic]));
}

#[test]
fn workflow_focus_survives_agent_restore_and_explicit_agent_selection_still_works() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let allowed = BTreeSet::from([session]);
    let terminals = [
        scoped_terminal_ref(workspace, Some(session)),
        scoped_terminal_ref(workspace, Some(session)),
    ];
    let agents = terminals
        .iter()
        .map(|terminal| AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
                .unwrap(),
            continuation: AgentContinuationRef::new(),
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        })
        .collect::<Vec<_>>();
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            allowed.clone(),
            Box::new(MemoryIntentPort {
                state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    runtime.on_effect(&Effect::OpenWorkflow { session });
    let workflow_selection = runtime.active_pane().selected().clone();
    // First Codex and then Claude appear through the real coherent restore path.
    for count in [1, 2] {
        let fence = runtime.restore_fence();
        let restored = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: allowed.clone(),
                terminals: Ok(terminals[..count]
                    .iter()
                    .map(|terminal| TerminalInventoryEntry {
                        terminal: terminal.clone(),
                        kind: TerminalKind::Agent,
                        live: true,
                    })
                    .collect()),
                agents: Ok(AgentInventory {
                    workspace_id: workspace,
                    runtimes: agents[..count].to_vec(),
                    resumable: Vec::new(),
                }),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &allowed,
        );
        assert_eq!(restored.outcome, super::RestoreJobOutcome::Applied);
        assert_eq!(runtime.active_pane().selected(), &workflow_selection);
        assert_eq!(runtime.active_pane().tabs().len(), count + 1);
        assert_eq!(runtime.focused_terminal(), None);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    // Explicit tab navigation remains the existing exact-terminal selection path.
    for terminal in &terminals {
        sender
            .send(ControllerHostAction::SelectTab(TabDirection::Next))
            .unwrap();
        drain_host_actions(
            &receiver,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(runtime.focused_terminal().as_ref(), Some(terminal));
    }
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.active_pane().selected(), &workflow_selection);
}

#[test]
#[allow(clippy::too_many_lines)] // One user flow covers close, inventory replay, explicit open, and exit cleanup.
fn generic_close_survives_inventory_replay_until_explicit_open() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = scoped_terminal_ref(workspace, Some(session));
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(WheelRecordingPort {
                terminal: terminal.clone(),
                replay: Vec::new(),
                inputs: Arc::new(Mutex::new(Vec::new())),
                input_error: false,
            }),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: durable,
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let completion =
        |live: bool, fence: (u64, u64), port: Box<dyn AgentCommandPort>| super::RestoreCompletion {
            port,
            dispatched_interaction: fence.0,
            dispatched_registry_revision: fence.1,
            dispatched_allowed_sessions: BTreeSet::from([session]),
            terminals: Ok(vec![TerminalInventoryEntry {
                terminal: terminal.clone(),
                kind: TerminalKind::Terminal,
                live,
            }]),
            agents: Ok(AgentInventory {
                workspace_id: workspace,
                runtimes: Vec::new(),
                resumable: Vec::new(),
            }),
            observation_coherent: true,
        };

    let first_fence = runtime.restore_fence();
    let first = super::apply_restore_completion(
        completion(true, first_fence, Box::new(UnavailableAgentCommandPort)),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(first.outcome, super::RestoreJobOutcome::Applied);
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    ui.start_terminal_session(terminal.clone(), terminal_geometry(20, 80));

    super::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.contains(&terminal));

    // The same live row may arrive from a restore already queued around the
    // close. Its exact process-local fence keeps the tab closed.
    let replay_fence = runtime.restore_fence();
    let replay = super::apply_restore_completion(
        completion(true, replay_fence, first.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(replay.outcome, super::RestoreJobOutcome::Applied);
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.contains(&terminal));

    // A completion outside the pending tab's target is rejected by the
    // runtime and must not release the close fence as a side effect.
    let refused_operation = OperationId::new();
    let target = Target::Session(session);
    let _ = runtime.request_pane(target, refused_operation, PaneKind::Terminal);
    let mut refused_pending = std::collections::HashMap::from([(refused_operation, target)]);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Terminal {
                operation: refused_operation,
                result: Ok(scoped_terminal_ref(workspace, Some(SessionId::new()))),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut refused_pending,
        terminal_geometry(20, 80),
    );
    assert!(ui.closed_generic_terminals.contains(&terminal));
    let _ = runtime.fail_pane(target, refused_operation, "wrong scope".to_owned());

    // An immediate `terminal open` may race the shell exit and resolve to
    // that exact daemon terminal. Reject it instead of resurrecting the
    // scrollback the user just closed.
    let operation = OperationId::new();
    let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Terminal {
                operation,
                result: Ok(terminal.clone()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );
    assert!(runtime.active_pane().tabs().is_empty());
    assert_eq!(
        runtime.active_pane().error(),
        Some("terminal is still closing; try again")
    );
    assert!(ui.closed_generic_terminals.contains(&terminal));

    // Coherent exit observation retires the fence. A later open can then
    // accept only the fresh daemon terminal and its blank scrollback.
    let exit_fence = runtime.restore_fence();
    let exited = super::apply_restore_completion(
        completion(false, exit_fence, replay.port),
        &mut ui,
        &mut runtime,
        workspace,
        &BTreeSet::from([session]),
    );
    assert_eq!(exited.outcome, super::RestoreJobOutcome::Applied);
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(ui.closed_generic_terminals.is_empty());

    let fresh = scoped_terminal_ref(workspace, Some(session));
    let operation = OperationId::new();
    let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
    let mut pending = std::collections::HashMap::from([(operation, target)]);
    ui.pane_completion_sender
        .send(super::PaneLaunchCompletion {
            launch_id: super::PANE_LAUNCH_UNADMITTED,
            outcome: super::PaneLaunchOutcome::Terminal {
                operation,
                result: Ok(fresh.clone()),
            },
        })
        .unwrap();
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );
    assert_eq!(runtime.focused_terminal(), Some(fresh));
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture closes, replays, reconnects, and reopens.
fn closing_an_unobserved_live_agent_survives_inventory_replay_and_reconnect() {
    let workspace = WorkspaceId::new();
    let closed_terminal = scoped_terminal_ref(workspace, None);
    let surviving_terminal = scoped_terminal_ref(workspace, None);
    // Nothing is saved yet, so both root conversations are projected from
    // their terminal fence alone (#599) and neither has a continuation.
    let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::new(),
            Box::new(MemoryIntentPort {
                state: Arc::clone(&durable),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    // The root target owns the Agent drawer, so it is the active pane only
    // while the drawer is open.
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Root(workspace),
            panes: vec![
                LivePane {
                    terminal: closed_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: surviving_terminal.clone(),
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(closed_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    assert_eq!(ui.agent_continuation_for(&closed_terminal), None);

    super::close_focused_terminal_pane(
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );

    assert_eq!(runtime.focused_terminal(), Some(closed_terminal));
    assert!(durable.lock().unwrap().dismissed.is_empty());
    assert!(durable.lock().unwrap().dismissed_terminals.is_empty());
    assert_eq!(
        runtime.active_pane().error(),
        Some("terminal session is no longer available")
    );
    assert!(runtime.state().notice().is_none());
}

#[test]
#[allow(clippy::too_many_lines)] // Agent and generic routing share one persistence-failure fixture.
fn persistence_failures_block_agent_reorder_and_selection_but_not_generic_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let mut intent = AgentTabIntent::empty(workspace);
    for (continuation, terminal, select) in [
        (first, first_terminal.clone(), true),
        (second, second_terminal.clone(), false),
    ] {
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal,
            select,
        });
    }
    let durable = Arc::new(Mutex::new(intent));
    let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::clone(&durable),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&attempts),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second_terminal.clone(),
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first_terminal.clone()),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let _ = runtime.handle_key(Key::Enter);
    let tabs_before = runtime.active_pane().tabs().to_vec();
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabNext),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut std::collections::HashMap::new(),
        20,
        80,
        0,
        0,
    ));
    assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabPrevious),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut std::collections::HashMap::new(),
        20,
        80,
        0,
        0,
    ));
    assert_eq!(runtime.active_pane().tabs(), tabs_before.as_slice());

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut ui,
        &mut runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(runtime.focused_terminal(), Some(first_terminal));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        serde_json::to_vec(&*durable.lock().unwrap()).unwrap(),
        bytes_before
    );
    assert_eq!(
        runtime
            .state()
            .notice()
            .map(|notice| notice.message.as_str()),
        Some(AgentTabIntentError::Unavailable.safe_message())
    );

    // A generic-only pane has no Agent intent to persist, so the same
    // unavailable store cannot regress its normal tab controls.
    let generic_first = scoped_terminal_ref(workspace, Some(session));
    let generic_second = scoped_terminal_ref(workspace, Some(session));
    let empty = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
    let generic_attempts = Arc::new(AtomicUsize::new(0));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut generic_ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: empty,
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::clone(&generic_attempts),
            }),
        );
    let mut generic_runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let fence = generic_runtime.restore_fence();
    assert!(generic_runtime.restore_snapshot(
        fence.0,
        fence.1,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: generic_first.clone(),
                    kind: PaneKind::Terminal,
                },
                LivePane {
                    terminal: generic_second.clone(),
                    kind: PaneKind::Terminal,
                },
            ],
            selected: Some(generic_first),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    ));
    let _ = generic_runtime.handle_key(Key::Enter);
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabNext),
        &mut generic_ui,
        &mut generic_runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut std::collections::HashMap::new(),
        20,
        80,
        0,
        0,
    ));
    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(
        &receiver,
        &mut generic_ui,
        &mut generic_runtime,
        &mut std::collections::HashMap::new(),
    );
    assert_eq!(generic_attempts.load(Ordering::SeqCst), 0);
    assert!(generic_runtime.focused_terminal().is_some());
}

#[test]
#[allow(clippy::too_many_lines)] // Reorder success and both persistence failures share one stable fixture.
fn reorder_control_commits_agent_lineages_in_the_new_stable_order() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first_terminal = scoped_terminal_ref(workspace, Some(session));
    let second_terminal = scoped_terminal_ref(workspace, Some(session));
    let first = AgentContinuationRef::new();
    let second = AgentContinuationRef::new();
    let mutations = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(ScriptedAgentPort {
                terminal: first_terminal.clone(),
                subscription: 9,
                replay: Vec::new(),
                poll_error: None,
                detaches: Arc::new(Mutex::new(Vec::new())),
            }),
        )
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                mutations: Arc::clone(&mutations),
            }),
        );
    for (continuation, terminal) in [
        (first, first_terminal.clone()),
        (second, second_terminal.clone()),
    ] {
        let _ = ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal,
            select: false,
        });
    }
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (interaction, revision) = runtime.restore_fence();
    let _ = runtime.restore_snapshot(
        interaction,
        revision,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: vec![
                LivePane {
                    terminal: first_terminal.clone(),
                    kind: PaneKind::Agent,
                },
                LivePane {
                    terminal: second_terminal,
                    kind: PaneKind::Agent,
                },
            ],
            selected: Some(first_terminal),
            selected_interrupted: None,
            interrupted: Vec::new(),
        }],
    );
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: OperationId::new(),
        profile: None,
    });
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending = std::collections::HashMap::new();

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabNext),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        0,
        0,
    ));
    assert!(matches!(
        mutations.lock().unwrap().last(),
        Some(AgentTabIntentMutation::Reorder {
            session_id: Some(actual),
            continuations,
        }) if *actual == session && continuations == &[second, first]
    ));

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::MoveTabPrevious),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        0,
        0,
    ));
    assert!(matches!(
        mutations.lock().unwrap().last(),
        Some(AgentTabIntentMutation::Reorder {
            session_id: Some(actual),
            continuations,
        }) if *actual == session && continuations == &[first, second]
    ));
}

#[test]
fn restore_open_panes_projects_live_runtimes_and_skips_dead_and_duplicates() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_terminal = scoped_terminal_ref(workspace, Some(session));
    let root_agent = scoped_terminal_ref(workspace, Some(session));
    let session_terminal = scoped_terminal_ref(workspace, Some(session));
    let dead = scoped_terminal_ref(workspace, Some(session));
    let entries = vec![
        TerminalInventoryEntry {
            terminal: root_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: root_agent.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        // A dead process is reported non-live and must not become a tab.
        TerminalInventoryEntry {
            terminal: dead.clone(),
            kind: TerminalKind::Terminal,
            live: false,
        },
        // A duplicate of a live runtime must not double the tab.
        TerminalInventoryEntry {
            terminal: root_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
    ];
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries,
                fail: false,
                inputs: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);

    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));

    // All three managed-session runtimes are projected, with the duplicate
    // terminal removed.
    assert_eq!(runtime.active_pane().tabs().len(), 3);
    assert!(runtime.state().has_live_pane());
    // Every live runtime is attached and streaming; the dead one is not.
    assert!(ui.terminal_rows(&root_terminal, None).is_some());
    assert!(ui.terminal_rows(&root_agent, None).is_some());
    assert!(ui.terminal_rows(&session_terminal, None).is_some());
    assert!(ui.terminal_rows(&dead, None).is_none());
}

#[test]
fn restored_terminal_and_agent_tabs_deliver_ordinary_closeup_input() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = scoped_terminal_ref(workspace, Some(session));
    let agent = scoped_terminal_ref(workspace, Some(session));
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let entries = vec![
        TerminalInventoryEntry {
            terminal: terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: agent.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
    ];
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            Vec::new(),
            Box::new(RestoreInventoryPort {
                entries,
                fail: false,
                inputs: inputs.clone(),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();

    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    // Inventory restoration stays in Switch, but preselects the first tab so
    // entering Closeup has a concrete input owner instead of a target-only
    // selection hidden behind a non-empty tab strip.
    assert!(!runtime.wants_live_input());
    assert_eq!(runtime.focused_terminal(), Some(terminal.clone()));
    assert!(runtime.handle_key(Key::Enter).is_empty());
    assert!(runtime.wants_live_input());
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Char('x'),
    ));

    let _ = runtime.select_tab(TabDirection::Next);
    assert_eq!(runtime.focused_terminal(), Some(agent.clone()));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Enter,
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: b"copy".to_vec(),
        },
    ));
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![
            (terminal, b"x".to_vec()),
            (agent.clone(), b"\r".to_vec()),
            (agent, b"copy".to_vec()),
        ]
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One production-order fixture observes every reserved and passthrough branch.
fn production_input_order_reserves_drawer_picker_before_root_agent_pty() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_agent = scoped_terminal_ref(workspace, None);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries: vec![TerminalInventoryEntry {
                    terminal: root_agent.clone(),
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

    // This is the same production ordering used by the frame loop: the
    // closed drawer leaves the resolved chord for the reducer, which opens
    // both drawer and picker without sending anything to the managed pane.
    let new = Key::Live(LiveTerminalAction::DirectorNew);
    assert_eq!(
        route_workspace_input_before_reducer(&mut ui, &mut runtime, &mut controls, &mut term, &new,),
        WorkspaceInputRoute::Unhandled
    );
    assert!(runtime.handle_key(new).is_empty());
    assert!(runtime.state().director_drawer_open());
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(DefaultModel::Claude)
    ));
    assert_eq!(runtime.focused_terminal(), Some(root_agent.clone()));

    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::DirectorNew),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::Director),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(!runtime.state().director_drawer_open());
    let reopen = Key::Live(LiveTerminalAction::DirectorNew);
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &reopen,
        ),
        WorkspaceInputRoute::Unhandled
    );
    assert!(runtime.handle_key(reopen).is_empty());
    for key in [Key::Down, Key::Up] {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &key,
            ),
            WorkspaceInputRoute::Drawer(Vec::new())
        );
    }
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert_eq!(runtime.state().director_new(), DirectorNew::Idle);
    assert!(inputs.lock().unwrap().is_empty());

    for key in [
        Key::Up,
        Key::Down,
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Live(LiveTerminalAction::NextTab),
    ] {
        assert_eq!(
            route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                &mut term,
                &key,
            ),
            WorkspaceInputRoute::Unhandled
        );
    }

    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Char('x'),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Enter,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(inputs.lock().unwrap().is_empty());

    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::DirectorNew),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    let WorkspaceInputRoute::Drawer(effects) = route_workspace_input_before_reducer(
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &Key::Enter,
    ) else {
        panic!("picker Enter must stay in the drawer");
    };
    assert!(matches!(
        effects.as_slice(),
        [Effect::LaunchAgent {
            session: None,
            profile: Some(profile),
            ..
        }] if profile.as_str() == "claude"
    ));
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(runtime.state().director_drawer_open());
    assert!(runtime.state().director_launching().is_some());
    assert!(inputs.lock().unwrap().is_empty());
}

#[test]
fn work_run_chord_opens_only_the_goal_driven_run_control() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let run = observed_work_run(SupervisorRunState::Running);
    let runs = super::WorkRunProjection::fresh(vec![run.clone()]);
    let mut control = super::WorkRunControl::default();

    let input = super::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    )
    .expect("the Work Runs chord owns the input");
    assert!(input.effects.is_empty());
    assert_eq!(input.outcome, super::WorkRunControlOutcome::Consumed);
    assert!(runtime.state().director_drawer_open());
    assert_eq!(runtime.state().director_route(), DirectorRoute::WorkRuns);
    assert_eq!(control.mode(), super::WorkRunControlMode::List);
    assert_eq!(control.selected(), Some(run.supervisor_run_id));

    for key in [Key::Up, Key::Down, Key::Left, Key::Right] {
        assert!(
            super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &key,)
                .is_some()
        );
    }
    assert!(
        super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::Enter,)
            .is_some()
    );
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::RunOverview(run.supervisor_run_id)
    );
    assert!(
        super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::Escape,)
            .is_some()
    );
    assert_eq!(runtime.state().director_route(), DirectorRoute::WorkRuns);
    assert!(
        super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::CtrlX,)
            .is_some()
    );
    assert_eq!(control.feedback(), Some("Cancel the Work Run first"));
    assert!(
        super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::Resize,)
            .is_none()
    );
    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::Director),
        )
        .is_none()
    );
    assert_eq!(control.mode(), super::WorkRunControlMode::Closed);
    assert_eq!(control.selected(), Some(run.supervisor_run_id));
    assert!(
        super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::Resize,)
            .is_none()
    );

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);
    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::WorkRuns),
        )
        .is_none()
    );
    assert!(!runtime.state().director_drawer_open());
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    assert_eq!(control.mode(), super::WorkRunControlMode::Closed);
}

#[test]
fn goal_driven_work_runs_escape_closes_the_director_while_back_stays_at_root() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let runs = super::WorkRunProjection::fresh(Vec::new());
    let mut control = super::WorkRunControl::default();
    let _ = super::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    );

    let back = super::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::DirectorBack),
    )
    .expect("Work Runs owns Director back");
    assert_eq!(back.outcome, super::WorkRunControlOutcome::Consumed);
    assert!(runtime.state().director_drawer_open());
    assert_eq!(runtime.state().director_route(), DirectorRoute::WorkRuns);

    let escape =
        super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::Escape)
            .expect("Work Runs owns Escape");
    assert_eq!(escape.outcome, super::WorkRunControlOutcome::Consumed);
    assert!(!runtime.state().director_drawer_open());
    assert_eq!(runtime.state().director_route(), DirectorRoute::WorkRuns);
}

#[test]
fn work_run_surface_yields_new_and_fences_submitting_actions() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude]),
        DefaultModel::Claude,
    );
    let runs =
        super::WorkRunProjection::fresh(vec![observed_work_run(SupervisorRunState::Running)]);
    let mut control = super::WorkRunControl::default();
    let _ = super::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    );

    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_none(),
        "Director New must reach the drawer reducer"
    );
    assert_eq!(control.mode(), super::WorkRunControlMode::Closed);
    assert!(
        super::handle_director_picker_input(
            &mut runtime,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_some()
    );
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(_)
    ));

    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));
    control.open(runs.runs());
    let _ = control.handle(super::WorkRunControlAction::Cancel, runs.runs(), true);
    let submitted = control.handle(super::WorkRunControlAction::Enter, runs.runs(), true);
    assert!(submitted.into_request().is_some());
    assert_eq!(control.mode(), super::WorkRunControlMode::Submitting);
    let blocked = super::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::DirectorNew),
    )
    .expect("a durable action in flight owns Director New");
    assert_eq!(blocked.outcome, super::WorkRunControlOutcome::Consumed);
    assert_eq!(runtime.state().director_new(), DirectorNew::Idle);
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_some(),
        "the fence remains active after a Workflow route normalization"
    );
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    assert!(!runtime.state().director_drawer_open());
    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_some(),
        "a closed Director must not bypass the pending action fence"
    );
    assert!(!runtime.state().director_drawer_open());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Director));
    let drawer = super::director_drawer::geometry(20, 80);
    let start = Key::Click {
        column: u16::try_from(drawer.left + drawer.width - 3).unwrap(),
        row: u16::try_from(drawer.top + 2).unwrap(),
    };
    assert!(
        super::open_director_from_new_button(&mut runtime, &start, 20, 80, control.mode(),)
            .is_none(),
        "the Start button is inert while a durable action is in flight"
    );
}

#[test]
fn focused_shell_outweighs_background_work_run_control() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let runs =
        super::WorkRunProjection::fresh(vec![observed_work_run(SupervisorRunState::Running)]);
    let mut control = super::WorkRunControl::default();
    let _ = super::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    );
    let _ = control.handle(super::WorkRunControlAction::Cancel, runs.runs(), true);
    let submitted = control.handle(super::WorkRunControlAction::Enter, runs.runs(), true);
    assert!(submitted.into_request().is_some());

    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut control,
            &runs,
            &Key::Live(LiveTerminalAction::DirectorNew),
        )
        .is_none(),
        "a background Work Run submission must not consume Shell New"
    );
    assert_eq!(control.mode(), super::WorkRunControlMode::Submitting);
    let mut shell_control = super::WorkRunControl::default();
    shell_control.open(runs.runs());
    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut shell_control,
            &runs,
            &Key::Char('x'),
        )
        .is_none(),
        "an unfocused Director must not consume Shell input"
    );
    assert_eq!(shell_control.mode(), super::WorkRunControlMode::Closed);
    assert_eq!(
        super::workspace_foreground_input_owner(&runtime),
        super::WorkspaceForegroundInputOwner::Downstream
    );
}

#[test]
#[allow(clippy::too_many_lines)] // This table drives every keyboard edge of the retained run surface.
fn work_run_routes_confirmations_and_console_activation_without_implicit_mutation() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);
    let running = observed_work_run(SupervisorRunState::Running);
    let runs = super::WorkRunProjection::fresh(vec![running.clone()]);
    let mut control = super::WorkRunControl::default();
    let _ = super::handle_work_run_control_input(
        &mut runtime,
        &mut control,
        &runs,
        &Key::Live(LiveTerminalAction::WorkRuns),
    );

    let _ = control.handle(super::WorkRunControlAction::Cancel, runs.runs(), true);
    for key in [Key::Up, Key::Down, Key::Left, Key::Right, Key::CtrlX] {
        let input =
            super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &key).unwrap();
        assert_eq!(input.outcome, super::WorkRunControlOutcome::Consumed);
    }
    let _ = super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::Quit);
    assert_eq!(control.mode(), super::WorkRunControlMode::List);

    for key in [Key::Escape, Key::Live(LiveTerminalAction::DirectorBack)] {
        let _ = control.handle(super::WorkRunControlAction::Cancel, runs.runs(), true);
        let _ = super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &key);
        assert_eq!(control.mode(), super::WorkRunControlMode::List);
    }
    let _ = control.handle(super::WorkRunControlAction::Cancel, runs.runs(), true);
    let submitted =
        super::handle_work_run_control_input(&mut runtime, &mut control, &runs, &Key::Enter)
            .unwrap();
    assert!(submitted.outcome.into_request().is_some());

    let mut empty_control = super::WorkRunControl::default();
    let empty = super::WorkRunProjection::fresh(Vec::new());
    empty_control.open(empty.runs());
    let _ =
        super::handle_work_run_control_input(&mut runtime, &mut empty_control, &empty, &Key::Enter);
    assert_eq!(empty_control.feedback(), Some("No Work Run is selected"));

    let mut inert_control = super::WorkRunControl::default();
    inert_control.open(runs.runs());
    for route in [
        DirectorRoute::Organization,
        DirectorRoute::Console(DirectorConsoleParent::Organization),
    ] {
        let input = super::handle_work_run_list_input(
            None,
            &mut runtime,
            &mut inert_control,
            &runs,
            &Key::Enter,
            route,
            Vec::new(),
        );
        assert_eq!(input.outcome, super::WorkRunControlOutcome::Consumed);
    }
    let _ = super::handle_work_run_list_input(
        None,
        &mut runtime,
        &mut inert_control,
        &runs,
        &Key::Quit,
        DirectorRoute::WorkRuns,
        Vec::new(),
    );
    assert_eq!(
        inert_control.mode(),
        super::WorkRunControlMode::ConfirmCancel
    );

    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::Classic);
    let mut suspended = super::WorkRunControl::default();
    suspended.open(runs.runs());
    assert!(
        super::handle_work_run_control_input(&mut runtime, &mut suspended, &runs, &Key::Other,)
            .is_none()
    );
    assert_eq!(suspended.mode(), super::WorkRunControlMode::Closed);
    runtime.set_work_mode(usagi_core::domain::settings::WorkMode::GoalDriven);

    let terminal = scoped_terminal_ref(workspace, None);
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Agent);
    let _ = runtime.complete_pane(Target::Root(workspace), operation, terminal.clone());
    let runtime_id = AgentRuntimeId::new();
    let continuation = AgentContinuationRef::new();
    let mut run_with_director = running;
    run_with_director.root_agent_id = Some(runtime_id);
    let run_id = run_with_director.supervisor_run_id;
    let overview_runs = super::WorkRunProjection::fresh(vec![run_with_director]);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    ui.agent_inventory = Some(AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(runtime_id, terminal, None).unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    });
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorRunOverview(run_id)));
    let mut overview_control = super::WorkRunControl::default();
    overview_control.open(overview_runs.runs());
    let activated = super::handle_work_run_control_input_with_ui(
        Some(&mut ui),
        &mut runtime,
        &mut overview_control,
        &overview_runs,
        &Key::Enter,
    )
    .unwrap();
    assert!(activated.effects.is_empty());
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Console(DirectorConsoleParent::RunOverview(run_id))
    );
    assert!(
        super::handle_work_run_control_input(
            &mut runtime,
            &mut overview_control,
            &overview_runs,
            &Key::Other,
        )
        .is_none(),
        "the Console owns input after leaving the Run Overview"
    );
    assert_eq!(overview_control.mode(), super::WorkRunControlMode::Closed);

    let no_root = observed_work_run(SupervisorRunState::Running);
    let no_root_id = no_root.supervisor_run_id;
    let no_root_runs = super::WorkRunProjection::fresh(vec![no_root]);
    let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorRunOverview(no_root_id)));
    let mut unavailable_control = super::WorkRunControl::default();
    unavailable_control.open(no_root_runs.runs());
    let _ = super::handle_work_run_control_input(
        &mut runtime,
        &mut unavailable_control,
        &no_root_runs,
        &Key::Enter,
    );
    assert_eq!(
        unavailable_control.feedback(),
        Some("Director Console is not available for this Work Run")
    );
}

#[test]
fn director_selection_rejects_placeholders_and_surfaces_intent_failure() {
    let workspace = WorkspaceId::new();
    let terminal = scoped_terminal_ref(workspace, None);
    let runtime_id = AgentRuntimeId::new();
    let continuation = AgentContinuationRef::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Agent);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    assert!(!super::select_director_selection(
        TabSelection::Pending(operation),
        &mut ui,
        &mut runtime,
    ));
    assert!(!super::select_director_selection(
        TabSelection::Ready(operation),
        &mut ui,
        &mut runtime,
    ));
    assert!(super::select_director_selection(
        TabSelection::Interrupted(continuation),
        &mut ui,
        &mut runtime,
    ));
    assert!(!super::select_director_agent(
        runtime_id,
        &mut ui,
        &mut runtime,
    ));

    let _ = runtime.complete_pane(Target::Root(workspace), operation, terminal.clone());
    ui.agent_inventory = Some(AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(runtime_id, terminal.clone(), None).unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    });
    let mut intent = AgentTabIntent::empty(workspace);
    intent.apply(AgentTabIntentMutation::Upsert {
        session_id: None,
        continuation,
        terminal: terminal.clone(),
        select: true,
    });
    ui = ui.with_agent_tab_intent(
        workspace,
        BTreeSet::new(),
        Box::new(FailingIntentPort {
            state: Arc::new(Mutex::new(intent)),
            error: AgentTabIntentError::Unavailable,
            attempts: Arc::new(AtomicUsize::new(0)),
        }),
    );
    assert!(!super::select_director_selection(
        TabSelection::Live(terminal.clone()),
        &mut ui,
        &mut runtime,
    ));
    assert!(!super::select_director_agent(
        runtime_id,
        &mut ui,
        &mut runtime,
    ));
}

/// `Esc` belongs to the drawer's selected root Agent — an agent CLI reads it
/// as its own interrupt — so the drawer keeps it only when no live
/// conversation can receive it. `Ctrl-O Ctrl-G` closes the drawer either way.
#[test]
fn drawer_escape_reaches_the_selected_root_agent_and_closes_only_without_one() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_agent = scoped_terminal_ref(workspace, None);
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries: vec![TerminalInventoryEntry {
                    terminal: root_agent.clone(),
                    kind: TerminalKind::Agent,
                    live: true,
                }],
                fail: false,
                inputs: Arc::clone(&inputs),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();

    let open = Key::Live(LiveTerminalAction::Director);
    assert!(runtime.handle_key(open).is_empty());
    assert!(runtime.state().director_drawer_open());
    assert_eq!(runtime.focused_terminal(), Some(root_agent.clone()));
    assert_eq!(
        runtime.state().director_route(),
        DirectorRoute::Organization
    );
    assert!(runtime.handle_key(Key::Enter).is_empty());

    // The live conversation owns Esc: it reaches the PTY once and the drawer
    // stays open.
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Forwarded
    );
    assert!(runtime.state().director_drawer_open());
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![(root_agent.clone(), vec![0x1b])]
    );

    // Closing stays reachable through the drawer's own chord.
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &Key::Live(LiveTerminalAction::Director),
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(!runtime.state().director_drawer_open());
    assert_eq!(*inputs.lock().unwrap(), vec![(root_agent, vec![0x1b])]);

    // With no conversation to receive it, Esc keeps its drawer meaning.
    let empty_view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut empty_ui = WorkspaceIoRuntime::new(empty_view, Box::new(UnavailableSessionCommandPort));
    let mut empty_runtime = WorkspaceRuntime::new(workspace, vec![session]);
    assert!(
        empty_runtime
            .handle_key(Key::Live(LiveTerminalAction::Director))
            .is_empty()
    );
    assert!(empty_runtime.state().director_drawer_open());
    assert_eq!(empty_runtime.focused_terminal(), None);
    assert_eq!(
        route_workspace_input_before_reducer(
            &mut empty_ui,
            &mut empty_runtime,
            &mut controls,
            &mut term,
            &Key::Escape,
        ),
        WorkspaceInputRoute::Drawer(Vec::new())
    );
    assert!(!empty_runtime.state().director_drawer_open());
}

/// The classifier already maps both forms of `n` to New and both forms of
/// `f` to Next. Director keeps those actions unchanged; only the workspace
/// terminal retargets New to a new terminal tab.
#[test]
fn drawer_context_keeps_new_and_next_distinct_after_modifier_normalization() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    runtime.set_agent_models(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::OpenAi]),
        DefaultModel::Claude,
    );

    // Closed drawer: every key passes through unchanged.
    for key in [
        Key::Live(LiveTerminalAction::NextTab),
        Key::Live(LiveTerminalAction::DirectorNew),
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Escape,
    ] {
        assert_eq!(retarget_drawer_chords(&runtime, key.clone()), key);
    }

    assert!(
        runtime
            .handle_key(Key::Live(LiveTerminalAction::Director))
            .is_empty()
    );
    assert!(runtime.state().director_drawer_open());

    // Director keeps the normalized actions unchanged.
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::NextTab)),
        Key::Live(LiveTerminalAction::NextTab)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew)),
        Key::Live(LiveTerminalAction::DirectorNew)
    );
    for key in [
        Key::Live(LiveTerminalAction::PreviousTab),
        Key::Live(LiveTerminalAction::Director),
        Key::Escape,
        Key::Char('n'),
    ] {
        assert_eq!(retarget_drawer_chords(&runtime, key.clone()), key);
    }

    // New reaches the reducer without a context-specific swap.
    let retargeted = retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew));
    assert!(runtime.handle_key(retargeted).is_empty());
    assert!(matches!(
        runtime.state().director_new(),
        DirectorNew::Choosing(DefaultModel::Claude)
    ));

    // Director remains visible behind Shell, but its chord vocabulary no
    // longer applies while Shell owns root input.
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    assert!(runtime.state().director_drawer_open());
    assert_eq!(
        runtime.state().workspace_drawer_focus(),
        Some(WorkspaceDrawerFocus::Terminal)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::NextTab)),
        Key::Live(LiveTerminalAction::NextTab)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew)),
        Key::Live(LiveTerminalAction::NewRootTerminal)
    );
}

#[test]
fn root_terminal_drawer_retargets_plain_new_to_a_terminal_tab() {
    let workspace = WorkspaceId::new();
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));

    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::DirectorNew)),
        Key::Live(LiveTerminalAction::NewRootTerminal)
    );
    assert_eq!(
        retarget_drawer_chords(&runtime, Key::Live(LiveTerminalAction::NextTab)),
        Key::Live(LiveTerminalAction::NextTab)
    );
}

#[test]
fn root_terminal_drawer_cycles_and_clicks_terminal_only_tabs() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let managed = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        managed.clone(),
        Box::new(WheelRecordingPort {
            terminal: managed,
            replay: Vec::new(),
            inputs: Arc::new(Mutex::new(Vec::new())),
            input_error: false,
        }),
    );
    let first = scoped_terminal_ref(workspace, None);
    let second = scoped_terminal_ref(workspace, None);
    for terminal in [&first, &second] {
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Root(workspace), operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(Target::Root(workspace), operation, terminal.clone());
    }
    let _ = runtime.handle_key(Key::Live(LiveTerminalAction::RootTerminal));
    let _ = runtime.select_tab_selection(TabSelection::Live(first.clone()));

    assert!(!select_root_terminal_tab(&Key::Other, &mut runtime));
    assert!(select_root_terminal_tab(
        &Key::Live(LiveTerminalAction::NextTab),
        &mut runtime,
    ));
    assert_eq!(runtime.focused_terminal(), Some(second.clone()));
    assert!(select_root_terminal_tab(
        &Key::Live(LiveTerminalAction::PreviousTab),
        &mut runtime,
    ));
    assert_eq!(runtime.focused_terminal(), Some(first));
    assert!(!select_root_terminal_tab(
        &Key::Live(LiveTerminalAction::OpenPullRequests),
        &mut runtime,
    ));

    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();
    let tab_row =
        u16::try_from(crate::presentation::views::root_terminal_drawer::geometry(30, 100).top + 2)
            .unwrap();
    assert!(intercept_live_terminal_control(
        &Key::Click {
            column: 15,
            row: tab_row,
        },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        30,
        100,
        0,
        0,
    ));
    assert_eq!(runtime.focused_terminal(), Some(second));
    assert!(!intercept_live_terminal_control(
        &Key::Click {
            column: 29,
            row: tab_row,
        },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        30,
        100,
        0,
        0,
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
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
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
fn double_clicked_append_restored_session_attaches_and_receives_input() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let root_terminal = scoped_terminal_ref(workspace, None);
    let session_terminal = scoped_terminal_ref(workspace, Some(session));
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let entries = vec![
        TerminalInventoryEntry {
            terminal: root_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        },
        TerminalInventoryEntry {
            terminal: session_terminal.clone(),
            kind: TerminalKind::Agent,
            live: true,
        },
    ];
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries,
                fail: false,
                inputs: inputs.clone(),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.append_restore_snapshot(
        interaction,
        revision,
        vec![
            super::PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: vec![LivePane {
                    terminal: root_terminal,
                    kind: PaneKind::Terminal,
                }],
                selected: None,
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
            super::PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: session_terminal.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: None,
                selected_interrupted: None,
                interrupted: Vec::new(),
            },
        ],
    ));
    let _ = runtime.apply_event(AppEvent::Resize {
        width: 100,
        height: 30,
    });
    for at in [1_000, 1_100] {
        let _ = runtime.apply_event(AppEvent::Pointer {
            column: 5,
            row: 2,
            at: std::time::Duration::from_millis(at),
        });
    }
    ui.sync_foreground_terminal(
        runtime.focused_terminal().as_ref(),
        terminal_geometry(20, 80),
    );

    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Char('x'),
    ));
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![(session_terminal, b"x".to_vec())]
    );
}

#[test]
fn restore_open_panes_restores_nothing_on_daemon_failure_or_without_a_port() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let live = TerminalInventoryEntry {
        terminal: scoped_terminal_ref(workspace, None),
        kind: TerminalKind::Terminal,
        live: true,
    };

    // A daemon failure restores nothing (and never spawns locally).
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(RestoreInventoryPort {
                entries: vec![live],
                fail: true,
                inputs: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    assert!(runtime.active_pane().tabs().is_empty());
    assert!(!runtime.state().has_live_pane());

    // An embedder with no Agent port simply finds nothing to restore.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    restore_open_panes(&mut ui, &mut runtime, terminal_geometry(20, 80));
    assert!(runtime.active_pane().tabs().is_empty());
}

#[test]
fn a_live_terminal_drag_selects_and_release_copies_to_the_clipboard() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 9,
            replay: b"hello".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let rows_len = ui
        .terminal_rows(&terminal, None)
        .expect("attached live rows")
        .len();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));

    // The right pane starts at column 37 (36-wide sidebar + divider) and its
    // content begins at frame row 5. Drag across "hello" and release.
    let drag = |column| PointerEvent {
        kind: PointerKind::Drag,
        column,
        row: 5,
    };
    assert!(handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 37,
            row: 5,
        },
    ));
    assert!(!controls.has_selection());
    // The next drag report lands at the final "o". The press cell above is
    // still part of the copied range, so this must yield all of "hello".
    handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        drag(41),
    );
    handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Up,
            column: 41,
            row: 5,
        },
    );

    assert_eq!(term.copied, vec!["hello".to_owned()]);
    // The completed selection is retained, so the native copy shortcut can
    // copy it again without needing another mouse release.
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: Vec::new(),
        },
    ));
    assert_eq!(term.copied, vec!["hello".to_owned(), "hello".to_owned()]);
    // Releasing the mouse keeps the range highlighted instead of clearing it,
    // and the projected rows still carry the reverse-video selection.
    assert!(controls.has_selection());
    assert!(!controls.is_dragging());
    let projected = ui
        .terminal_rows(&terminal, controls.selection())
        .expect("selection rows");
    assert!(
        projected.iter().any(|row| row.contains("\u{1b}[7mhello")),
        "selection highlight lost after release: {projected:?}"
    );
    // A drag that copied a selection never also opens a link.
    assert!(browser.opened.is_empty());
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
        (agent.clone(), PaneKind::Agent),
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
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
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
fn retained_terminal_selection_copy_reports_missing_or_empty_selection() {
    let mut term = FakeTerminal::default();
    let mut controls = LiveTerminalControls::default();

    copy_terminal_selection(&mut controls, &mut term);
    assert_eq!(term.copied, Vec::<String>::new());
    assert_eq!(
        controls.project(Vec::new(), 1).feedback.as_deref(),
        Some("no terminal text is selected")
    );

    controls.begin_selection(TerminalSelection::begin(
        vec!["text".to_owned()],
        TerminalPoint { row: 0, column: 4 },
    ));
    copy_terminal_selection(&mut controls, &mut term);
    assert_eq!(term.copied, Vec::<String>::new());
    assert_eq!(
        controls.project(Vec::new(), 1).feedback.as_deref(),
        Some("no terminal text is selected")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // The pointer boundary matrix shares one geometry fixture.
fn pointer_classifier_covers_inert_scroll_drag_and_click_boundaries() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal,
            subscription: 30,
            replay: b"hello".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let inactive = WorkspaceRuntime::new(workspace, vec![session]);
    assert!(!forward_live_terminal_input(
        &mut ui,
        &inactive,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: Vec::new(),
        },
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: Vec::new(),
        },
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::TerminalCopy {
            fallback: b"fail".to_vec(),
        },
    ));
    assert!(forward_live_terminal_input(
        &mut ui,
        &runtime,
        &mut controls,
        &mut term,
        &Key::Passthrough(b"fail".to_vec()),
    ));
    let _ = poll_and_project_terminals(
        &mut ui,
        &mut runtime,
        &mut controls,
        Geometry { cols: 43, rows: 13 },
    );

    handle_terminal_pointer(
        &ui,
        &inactive,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Drag,
            column: 40,
            row: 5,
        },
    );
    handle_terminal_pointer(
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
            kind: PointerKind::Drag,
            column: 0,
            row: 0,
        },
    );
    handle_terminal_pointer(
        &ui,
        &inactive,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Up,
            column: 40,
            row: 5,
        },
    );
    handle_terminal_pointer(
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
            kind: PointerKind::Up,
            column: 0,
            row: 0,
        },
    );
    // A focus change or an out-of-content release after a valid press
    // consumes the gesture without opening or copying.
    for (release_runtime, column, row) in [(&inactive, 40, 5), (&runtime, 0, 0)] {
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
                column: 40,
                row: 5,
            },
        ));
        assert!(handle_terminal_pointer(
            &ui,
            release_runtime,
            &mut controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind: PointerKind::Up,
                column,
                row,
            },
        ));
    }
    for column in [40, 41] {
        handle_terminal_pointer(
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
                kind: PointerKind::Drag,
                column,
                row: 5,
            },
        );
    }
    assert!(!handle_terminal_pointer(
        &ui,
        &inactive,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 40,
            row: 5,
        },
    ));
    assert!(!handle_terminal_pointer(
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
            column: 0,
            row: 0,
        },
    ));
    let empty_view = WorkspaceView::with_runtime_ids(ws("empty"), empty_state("empty"), vec![]);
    let empty_ui = WorkspaceIoRuntime::new(empty_view, Box::new(UnavailableSessionCommandPort));
    let mut detached_controls = LiveTerminalControls::default();
    detached_controls.sync_focus(runtime.focused_terminal().as_ref());
    detached_controls.press_pointer(TerminalSelection::begin(
        vec!["detached".to_owned()],
        TerminalPoint { row: 0, column: 0 },
    ));
    assert!(handle_terminal_pointer(
        &empty_ui,
        &runtime,
        &mut detached_controls,
        &mut term,
        &mut browser,
        20,
        80,
        1,
        0,
        PointerEvent {
            kind: PointerKind::Up,
            column: 40,
            row: 5,
        },
    ));
    assert!(!handle_terminal_pointer(
        &empty_ui,
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
            column: 40,
            row: 5,
        },
    ));
    let mut empty_controls = LiveTerminalControls::default();
    for kind in [PointerKind::Drag, PointerKind::Up] {
        handle_terminal_pointer(
            &empty_ui,
            &runtime,
            &mut empty_controls,
            &mut term,
            &mut browser,
            20,
            80,
            1,
            0,
            PointerEvent {
                kind,
                column: 40,
                row: 5,
            },
        );
    }

    let mut pending = std::collections::HashMap::new();
    for key in [
        Key::Live(LiveTerminalAction::ScrollUp),
        Key::Live(LiveTerminalAction::ScrollDown),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Drag,
            column: 0,
            row: 0,
        }),
        Key::Click { column: 0, row: 0 },
    ] {
        let _ = intercept_live_terminal_control(
            &key,
            &mut ui,
            &mut runtime,
            &mut controls,
            &mut term,
            &mut browser,
            &mut pending,
            20,
            80,
            1,
            0,
        );
    }
}

/// A recording [`BrowserOpener`] fake: it captures opened URLs so a pointer
/// test can assert what (if anything) a click launched, and never runs IO.
#[derive(Default)]
struct RecordingBrowser {
    opened: Vec<String>,
}

impl BrowserOpener for RecordingBrowser {
    fn open(&mut self, url: &str) -> Result<(), String> {
        self.opened.push(url.to_owned());
        Ok(())
    }
}

#[test]
fn a_down_up_click_on_a_terminal_link_opens_it_without_touching_the_pty() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 11,
            replay: b"see https://example.com/x now".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let rows_len = ui
        .terminal_rows(&terminal, None)
        .expect("attached live rows")
        .len();
    let mut term = FakeTerminal::default();
    let mut browser = RecordingBrowser::default();
    let mut controls = LiveTerminalControls::default();
    let mut pending_targets = std::collections::HashMap::new();
    controls.sync_focus(Some(&terminal));

    // A press-release with no drag: the URL starts at content column 4, so
    // frame column 37 + 4 = 41 lands on it. Down must not create the
    // one-cell selection that previously stole the release from link-open.
    assert!(intercept_live_terminal_control(
        &Key::Click { column: 41, row: 5 },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert!(!controls.has_selection());
    assert!(intercept_live_terminal_control(
        &Key::Pointer(PointerEvent {
            kind: PointerKind::Up,
            column: 41,
            row: 5,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert_eq!(browser.opened, vec!["https://example.com/x".to_owned()]);
    // A pointer release is not keyboard input, so nothing was forwarded to the
    // child PTY, and the clipboard was left alone.
    assert!(term.copied.is_empty());

    // A complete click on the leading prose (frame column 37 = content
    // column 0) opens nothing.
    assert!(intercept_live_terminal_control(
        &Key::Click { column: 37, row: 5 },
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert!(intercept_live_terminal_control(
        &Key::Pointer(PointerEvent {
            kind: PointerKind::Up,
            column: 37,
            row: 5,
        }),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending_targets,
        20,
        80,
        rows_len,
        0,
    ));
    assert_eq!(browser.opened.len(), 1);
}

#[test]
fn a_terminal_press_waits_for_drag_and_then_anchors_at_its_start_cell() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 9,
            replay: b"hello".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let rows_len = ui
        .terminal_rows(&terminal, None)
        .expect("attached live rows")
        .len();
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));
    // The right pane starts at column 37 and terminal content at row 5. The
    // press anchors the selection at the first "h", before the first drag
    // report reaches the controller.
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
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 37,
            row: 5,
        },
    ));
    assert!(!controls.is_dragging());
    assert!(!controls.has_selection());
    handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Drag,
            column: 38,
            row: 5,
        },
    );
    assert!(controls.is_dragging());
    assert_eq!(
        controls.selection().expect("selection started").anchor(),
        TerminalPoint { row: 0, column: 0 }
    );

    // A left-sidebar click remains with sidebar navigation; the terminal
    // interceptor must not consume it.
    assert!(!handle_terminal_pointer(
        &ui,
        &runtime,
        &mut controls,
        &mut term,
        &mut browser,
        20,
        80,
        rows_len,
        0,
        PointerEvent {
            kind: PointerKind::Down,
            column: 5,
            row: 2,
        },
    ));
}

#[test]
fn a_block_selection_over_padding_stays_visible_in_the_projected_rows() {
    // Regression: agents draw space-padded, mostly-blank screens. A block
    // drag across text, a blank line, and trailing padding must reach the
    // projected rows as reverse-video, not be trimmed into an invisible
    // selection (copy already worked from the snapshot cells).
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let (ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 9,
            replay: b"ab\r\n\r\ncd".to_vec(),
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let cells = ui.terminal_cells(&terminal).expect("attached cells");
    let mut selection = TerminalSelection::begin(cells, TerminalPoint { row: 0, column: 0 });
    selection.extend(TerminalPoint { row: 1, column: 5 });
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));
    controls.begin_selection(selection);
    let rows = controller_terminal_view(&ui, &runtime, &mut controls, 10)
        .expect("selection view")
        .rows;
    // Row 0's trailing padding and the blank row 1 are highlighted.
    assert!(
        rows[0].contains("\u{1b}[7m") && rows[0].contains("ab"),
        "row 0 padding not highlighted: {:?}",
        rows[0]
    );
    assert!(
        rows[1].contains("\u{1b}[7m"),
        "blank row 1 not highlighted: {:?}",
        rows[1]
    );
}

#[test]
fn agent_reply_auto_scroll_moves_the_retained_highlight_with_its_text() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let replay = b"\x1b[?1049hheader\x1b[13;1Hcomposer\x1b[2;3r\x1b[2;1Hone\r\ntwo".to_vec();
    let (mut ui, mut runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScrollingAgentPort {
            terminal: terminal.clone(),
            replay,
            output: Some(b"\x1b[1S\x1b[3;1Hreply".to_vec()),
        }),
    );
    let geometry = terminal_geometry(20, 80);
    let mut controls = LiveTerminalControls::default();
    // Drain the attach replacement before the user starts selecting, then
    // establish the alternate-buffer coordinate space from the first view.
    super::sync_terminal_selection_motions(&mut ui, &mut controls);
    let _ = controller_terminal_view(&ui, &runtime, &mut controls, usize::from(geometry.rows));
    let mut selection = ui
        .begin_terminal_selection(&terminal, TerminalPoint { row: 2, column: 0 })
        .expect("attached Agent screen");
    selection.extend(TerminalPoint { row: 2, column: 2 });
    controls.begin_selection(selection);
    assert_eq!(controls.finish_drag().as_deref(), Some("two"));

    let (view, _, _) = poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
    let rows = view.expect("live Agent view").rows;

    assert!(
        rows[1].contains("\x1b[7mtwo"),
        "Agent reply left the highlight at its old screen row: {rows:?}"
    );
    assert!(rows[2].contains("reply"));
    assert_eq!(
        controls.selection().map(TerminalSelection::text).as_deref(),
        Some("two")
    );
}

#[test]
fn a_selection_over_long_history_keeps_the_projection_viewport_bounded() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    let replay = (0..1_000)
        .flat_map(|line| format!("line {line}\r\n").into_bytes())
        .collect();
    let (ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 10,
            replay,
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let cells = ui.terminal_cells(&terminal).expect("attached cells");
    let last_row = cells.len().saturating_sub(1);
    let mut selection = TerminalSelection::begin(
        cells,
        TerminalPoint {
            row: last_row.saturating_sub(100),
            column: 0,
        },
    );
    selection.extend(TerminalPoint {
        row: last_row,
        column: 2,
    });
    let mut controls = LiveTerminalControls::default();
    controls.sync_focus(Some(&terminal));
    controls.begin_selection(selection);

    let viewport_rows = 20;
    let view = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
        .expect("selection view");
    assert!(view.total_rows > viewport_rows);
    assert_eq!(view.rows.len(), viewport_rows);
    assert_eq!(view.row_offset + view.rows.len(), view.total_rows);

    controls.scroll_up();
    let scrolled = controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
        .expect("scrolled selection view");
    assert_eq!(scrolled.rows.len(), viewport_rows);
    assert_eq!(scrolled.scroll, 1);
    assert_eq!(
        scrolled.row_offset + scrolled.rows.len() + scrolled.scroll,
        scrolled.total_rows
    );
}

#[test]
fn scrolling_a_live_terminal_offsets_its_projected_viewport() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal = live_terminal_ref(workspace, session);
    // Enough output to overflow the viewport so scrolling has headroom.
    let replay: Vec<u8> = (0..40)
        .flat_map(|line| format!("line {line}\r\n").into_bytes())
        .collect();
    let (ui, runtime) = focused_live_pane(
        workspace,
        session,
        terminal.clone(),
        Box::new(ScriptedAgentPort {
            terminal: terminal.clone(),
            subscription: 3,
            replay,
            poll_error: None,
            detaches: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let mut controls = LiveTerminalControls::default();
    let viewport_rows = usize::from(terminal_geometry(20, 80).rows);

    // The first projection anchors at the live bottom (scroll 0).
    let live_bottom =
        controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows).expect("live view");
    assert_eq!(live_bottom.scroll, 0);
    assert!(live_bottom.total_rows > live_bottom.rows.len());
    assert_eq!(live_bottom.rows.len(), viewport_rows);
    assert_eq!(
        live_bottom.row_offset + live_bottom.rows.len(),
        live_bottom.total_rows
    );
    controls.scroll_up();
    controls.scroll_up();
    let scrolled =
        controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows).expect("live view");
    assert_eq!(scrolled.scroll, 2);
    assert_eq!(
        scrolled.row_offset + scrolled.rows.len() + scrolled.scroll,
        scrolled.total_rows
    );
}

/// テスト用 Terminal。キー列を順に返し、描いたフレームを記録する。
#[derive(Default)]
struct FakeTerminal {
    keys: VecDeque<Key>,
    frames: Vec<Vec<String>>,
    waits: Vec<std::time::Duration>,
    copied: Vec<String>,
    size: Option<(usize, usize)>,
    create_call: Option<Receiver<String>>,
    observed_creates: Vec<String>,
    fail_size: bool,
    fail_draw: bool,
}

impl FakeTerminal {
    fn with_keys(keys: &[Key]) -> Self {
        Self {
            keys: keys.iter().cloned().collect(),
            ..Self::default()
        }
    }

    fn with_keys_waiting_for_create(keys: &[Key], create_call: Receiver<String>) -> Self {
        Self {
            create_call: Some(create_call),
            ..Self::with_keys(keys)
        }
    }
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_screen_graph_terminal_harness
impl Terminal for FakeTerminal {
    fn size(&mut self) -> io::Result<(usize, usize)> {
        if self.fail_size {
            return Err(io::Error::other("size failed"));
        }
        Ok(self.size.unwrap_or((0, 0)))
    }

    fn draw(&mut self, frame: &[String]) -> io::Result<()> {
        if self.fail_draw {
            return Err(io::Error::other("draw failed"));
        }
        self.frames.push(frame.to_vec());
        Ok(())
    }

    fn wait(&mut self, duration: std::time::Duration) -> io::Result<()> {
        self.waits.push(duration);
        Ok(())
    }

    fn read_key(&mut self) -> io::Result<Key> {
        let key = self
            .keys
            .pop_front()
            .ok_or_else(|| io::Error::other("no more keys"))?;
        // Create runs on the lifecycle worker. Tests that exercise the whole
        // terminal adapter wait at the quit boundary, making the dispatch
        // observation deterministic without changing production scheduling.
        if matches!(key, Key::CtrlQ)
            && let Some(create_call) = self.create_call.take()
        {
            let name = create_call
                .recv_timeout(std::time::Duration::from_secs(1))
                .map_err(|error| io::Error::other(error.to_string()))?;
            self.observed_creates.push(name);
        }
        Ok(key)
    }

    fn copy_text(&mut self, text: &str) -> Result<(), String> {
        self.copied.push(text.to_owned());
        Ok(())
    }
}

struct StaticMetrics;

impl MetricsPort for StaticMetrics {
    fn latest(&mut self) -> Option<DaemonMetrics> {
        Some(DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 42,
            active_subscribers: 3,
            dropped_updates: 0,
            cpu_percent_hundredths: 250,
            resident_memory_bytes: 45 * 1024 * 1024,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: None,
            failed_background_workers: 0,
        })
    }
}

struct StaticMetricsFactory;

impl MetricsPortFactory for StaticMetricsFactory {
    fn create(&mut self) -> Box<dyn MetricsPort> {
        Box::new(StaticMetrics)
    }
}

struct IdleAgentPort;

impl AgentCommandPort for IdleAgentPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("not launched in this test".to_owned())
    }
}

struct IdleAgentPortFactory;

impl AgentCommandPortFactory for IdleAgentPortFactory {
    fn create(&mut self) -> Box<dyn AgentCommandPort> {
        Box::new(IdleAgentPort)
    }
}

#[test]
fn idle_agent_port_is_safe_when_an_unexpected_launch_is_requested() {
    let mut port = IdleAgentPort;
    let error = port
        .launch(
            OperationId::new(),
            WorkspaceId::new(),
            Some(SessionId::new()),
            None,
        )
        .unwrap_err();

    assert_eq!(error, "not launched in this test");
    assert_eq!(
        port.launch_terminal(
            WorkspaceId::new(),
            Some(SessionId::new()),
            Geometry { cols: 80, rows: 24 },
            "open",
            OperationId::new(),
        )
        .unwrap_err(),
        "terminal launch is unavailable"
    );
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum FakeOpenSnapshot {
    #[default]
    Populated,
    Empty,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum FakeOperationMode {
    #[default]
    Inline,
    Background,
}

enum FakeRegistryRefresh {
    Queued(Vec<Workspace>),
    Pending(Vec<Workspace>),
}

#[derive(Default)]
struct FakeLoader {
    operation_mode: FakeOperationMode,
    opened: Vec<PathBuf>,
    refreshed: Vec<PathBuf>,
    open_snapshot: FakeOpenSnapshot,
    activate_error: Option<&'static str>,
    cleanup_removed: Vec<PathBuf>,
    cleanup_calls: usize,
    cleanup_candidates: Vec<Vec<PathBuf>>,
    missing: Vec<PathBuf>,
    missing_error: Option<io::ErrorKind>,
    missing_calls: Vec<Vec<PathBuf>>,
    unregistered: Vec<PathBuf>,
    unregister_calls: usize,
    created: Vec<NewRequest>,
    fail: bool,
    /// Stands in for the daemon refusing to adopt or describe the workspace
    /// being opened: the loader reports it as `PermissionDenied`, which
    /// entry screens present in place.
    refuse: Option<String>,
    /// Which paths `refuse` applies to. Empty means every path, so a fence
    /// that rejects only some registered workspaces can be expressed.
    refuse_paths: Vec<PathBuf>,
    /// Stands in for the daemon being unreachable while the workspace is
    /// opened: the loader reports it as `NotConnected`, which entry screens
    /// present in place rather than ending the process for.
    unreachable: Option<String>,
    /// Number of leading `create_workspace` calls that reject before the
    /// loader starts succeeding, standing in for a pre-flight rejection
    /// (e.g. the workspace already exists) that the user then corrects.
    create_failures: usize,
    create_completions: VecDeque<WorkspaceCreateCompletion>,
    held_create: Option<WorkspaceCreateCompletion>,
    hold_create: bool,
    dispatch_error: Option<&'static str>,
    release_after_polls: Option<usize>,
    completion_noise: bool,
    opened_at: Option<DateTime<Utc>>,
    open_delay: std::time::Duration,
    registry_refresh: Option<FakeRegistryRefresh>,
    registry_refresh_dispatches: usize,
    directory_entries: Vec<String>,
    directory_error: Option<io::ErrorKind>,
    directory_requests: Vec<PathBuf>,
}

impl WorkspaceLoader for FakeLoader {
    fn background_operations(&self) -> bool {
        self.operation_mode == FakeOperationMode::Background
    }

    fn open(&mut self, path: &Path) -> io::Result<WorkspaceSnapshot> {
        std::thread::sleep(self.open_delay);
        self.opened.push(path.to_path_buf());
        let fenced =
            self.refuse_paths.is_empty() || self.refuse_paths.iter().any(|fenced| fenced == path);
        if let Some(refusal) = self.refuse.as_ref().filter(|_| fenced) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                refusal.clone(),
            ));
        }
        if let Some(outage) = self.unreachable.as_ref().filter(|_| fenced) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, outage.clone()));
        }
        if self.fail {
            return Err(io::Error::other("open failed"));
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        let mut snapshot = snapshot(name);
        if self.open_snapshot == FakeOpenSnapshot::Empty {
            snapshot.state.sessions.clear();
            snapshot.session_ids.clear();
        }
        if let Some(opened_at) = self.opened_at {
            snapshot.workspace.updated_at = opened_at;
        }
        Ok(snapshot)
    }

    fn refresh(&mut self, path: &Path) -> io::Result<WorkspaceSnapshot> {
        self.refreshed.push(path.to_path_buf());
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        Ok(snapshot(name))
    }

    fn directory_names(&mut self, parent: &Path) -> io::Result<Vec<String>> {
        self.directory_requests.push(parent.to_path_buf());
        if let Some(kind) = self.directory_error {
            Err(io::Error::new(kind, "directory is unavailable"))
        } else {
            Ok(self.directory_entries.clone())
        }
    }

    fn record_unite(&mut self, _paths: &[PathBuf]) -> io::Result<()> {
        Ok(())
    }

    fn activate_prepared(&mut self, _path: &Path) -> io::Result<()> {
        self.activate_error
            .map_or(Ok(()), |error| Err(io::Error::other(error)))
    }

    fn dispatch_registry_refresh(&mut self) -> io::Result<bool> {
        match self.registry_refresh.take() {
            Some(FakeRegistryRefresh::Queued(registry)) => {
                self.registry_refresh = Some(FakeRegistryRefresh::Pending(registry));
                self.registry_refresh_dispatches += 1;
                Ok(true)
            }
            state => {
                self.registry_refresh = state;
                Ok(false)
            }
        }
    }

    fn take_registry_refresh(&mut self) -> Option<io::Result<Vec<Workspace>>> {
        match self.registry_refresh.take() {
            Some(FakeRegistryRefresh::Pending(registry)) => Some(Ok(registry)),
            queued => {
                self.registry_refresh = queued;
                None
            }
        }
    }

    fn missing_paths(&mut self, paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
        self.missing_calls.push(paths.to_vec());
        if let Some(kind) = self.missing_error {
            return Err(io::Error::new(kind, "workspace path could not be checked"));
        }
        Ok(paths
            .iter()
            .filter(|path| self.missing.contains(path))
            .cloned()
            .collect())
    }

    fn cleanup_missing(&mut self, workspaces: &[Workspace]) -> io::Result<Vec<PathBuf>> {
        self.cleanup_calls += 1;
        self.cleanup_candidates.push(
            workspaces
                .iter()
                .map(|workspace| workspace.path.clone())
                .collect(),
        );
        Ok(self.cleanup_removed.clone())
    }

    fn unregister(&mut self, paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
        self.unregister_calls += 1;
        self.unregistered.extend_from_slice(paths);
        Ok(paths.to_vec())
    }

    fn dispatch_create(&mut self, effect: WorkspaceCreateEffect) -> io::Result<()> {
        if let Some(error) = self.dispatch_error {
            return Err(io::Error::other(error));
        }
        self.created.push(effect.request.clone());
        let completion = if self.create_failures > 0 {
            self.create_failures -= 1;
            // Mirror the real loader's pre-flight rejection: no workspace is
            // created, so the caller keeps the draft and can retry.
            WorkspaceCreateCompletion {
                token: effect.token,
                request: effect.request.clone(),
                result: Err(io::Error::other(
                    "this directory is already a registered workspace",
                )),
            }
        } else {
            // Both modes resolve to a directory that is then opened like any
            // other workspace, mirroring the real loader.
            let path = match &effect.request {
                NewRequest::Clone { destination, .. } => destination.clone(),
                NewRequest::Existing { path, .. } => path.clone(),
            };
            let result = self.open(&path);
            WorkspaceCreateCompletion {
                token: effect.token,
                request: effect.request.clone(),
                result,
            }
        };
        if self.completion_noise {
            self.create_completions
                .push_back(WorkspaceCreateCompletion {
                    token: WorkspaceCreateToken::new(effect.token.get() + 100),
                    request: effect.request.clone(),
                    result: Err(io::Error::other("stale completion")),
                });
            self.create_completions
                .push_back(WorkspaceCreateCompletion {
                    token: effect.token,
                    request: NewRequest::Existing {
                        path: PathBuf::from("wrong-request"),
                        name: "wrong-request".to_owned(),
                    },
                    result: Err(io::Error::other("mismatched completion")),
                });
        }
        if self.hold_create {
            self.held_create = Some(completion);
        } else {
            self.create_completions.push_back(completion);
        }
        if self.completion_noise {
            self.create_completions
                .push_back(WorkspaceCreateCompletion {
                    token: effect.token,
                    request: effect.request,
                    result: Err(io::Error::other("duplicate completion")),
                });
        }
        Ok(())
    }

    fn take_create_completion(&mut self) -> Option<WorkspaceCreateCompletion> {
        if self.held_create.is_some()
            && let Some(remaining) = self.release_after_polls.as_mut()
        {
            if *remaining == 0 {
                self.create_completions
                    .push_back(self.held_create.take().expect("held create"));
            } else {
                *remaining -= 1;
            }
        }
        self.create_completions.pop_front()
    }
}

#[derive(Default)]
struct ResponsiveLoadingTerminal {
    keys: VecDeque<Key>,
    wait_keys: VecDeque<Key>,
    waits: Vec<std::time::Duration>,
    frames: Vec<Vec<String>>,
    draw_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl Terminal for ResponsiveLoadingTerminal {
    fn size(&mut self) -> io::Result<(usize, usize)> {
        Ok((24, 80))
    }

    fn draw(&mut self, frame: &[String]) -> io::Result<()> {
        self.frames.push(frame.to_vec());
        self.draw_count
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn wait(&mut self, duration: std::time::Duration) -> io::Result<()> {
        self.waits.push(duration);
        std::thread::sleep(duration.min(std::time::Duration::from_millis(2)));
        Ok(())
    }

    fn wait_for_key(&mut self, duration: std::time::Duration) -> io::Result<Option<Key>> {
        self.wait(duration)?;
        Ok(self.wait_keys.pop_front())
    }

    fn defer_key(&mut self, key: Key) {
        self.keys.push_back(key);
    }

    fn read_key(&mut self) -> io::Result<Key> {
        self.keys
            .pop_front()
            .ok_or_else(|| io::Error::other("no more keys"))
    }
}

#[test]
fn blocking_operations_keep_painting_and_workspace_open_can_be_cancelled() {
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::Escape, Key::Enter]),
        ..ResponsiveLoadingTerminal::default()
    };
    let draw_count = Arc::clone(&term.draw_count);

    let error = run_workspace_loading(&mut term, "Opening workspace…", true, || {
        while draw_count.load(std::sync::atomic::Ordering::Acquire) < 3 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    })
    .expect_err("escape cancels the visible wait");

    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    let painted = term.frames.iter().flatten().cloned().collect::<String>();
    assert!(painted.contains("Opening workspace…"));
    assert!(painted.contains("Cancelling…"));
    assert!(
        term.frames.len() >= 3,
        "the loading surface kept repainting"
    );
    assert_eq!(term.wait_keys, VecDeque::from([Key::Enter]));
}

#[test]
fn interruptible_loading_defers_non_escape_input_for_the_next_screen() {
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::CtrlQ]),
        ..ResponsiveLoadingTerminal::default()
    };
    let draw_count = Arc::clone(&term.draw_count);

    run_workspace_loading(&mut term, "Opening workspace…", true, || {
        while draw_count.load(std::sync::atomic::Ordering::Acquire) < 2 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    })
    .expect("a non-cancelling key does not interrupt the operation");

    assert_eq!(term.read_key().unwrap(), Key::CtrlQ);
}

#[test]
fn non_cancellable_loading_throttles_frames_without_consuming_input() {
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::Enter]),
        ..ResponsiveLoadingTerminal::default()
    };
    let draw_count = Arc::clone(&term.draw_count);

    run_workspace_loading(&mut term, "Saving settings…", false, || {
        while draw_count.load(std::sync::atomic::Ordering::Acquire) < 2 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    })
    .expect("non-cancellable save completes");

    assert!(
        term.frames.len() >= 2,
        "the loading surface kept repainting"
    );
    assert!(
        !term.waits.is_empty(),
        "the render loop yielded between frames"
    );
    assert!(
        term.waits
            .iter()
            .all(|duration| *duration == std::time::Duration::from_millis(80))
    );
    assert_eq!(term.wait_keys, VecDeque::from([Key::Enter]));
}

#[test]
fn workspace_switch_keeps_the_project_bar_and_cached_sessions_while_content_loads() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();

    let pending = cached_workspace_switch_frame(
        &deck,
        &beta.workspace.path,
        24,
        80,
        0,
        "Opening workspace…",
        false,
    )
    .expect("an already-open project has a local transition projection")
    .join("\n");
    let frame = cached_workspace_switch_frame(
        &deck,
        &beta.workspace.path,
        24,
        80,
        2,
        "Opening workspace…",
        true,
    )
    .expect("an already-open project has a local transition projection")
    .join("\n");

    assert!(frame.contains("1 alpha"));
    assert!(frame.contains("2 beta"));
    assert!(frame.contains("beta-session"));
    assert!(!frame.contains("alpha-session"));
    assert!(frame.contains("Opening workspace…"));
    assert!(pending.contains("beta-session"));
    assert!(!pending.contains("Opening workspace…"));
}

#[test]
fn workspace_switch_restores_each_projects_last_session_cursor() {
    let alpha = snapshot("alpha");
    let beta = snapshot_with_sessions("beta", &["first", "remembered"]);
    let remembered = beta.session_ids[1];
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
    deck.set_icon_mode(usagi_core::domain::settings::IconMode::Text);
    let mut previous = WorkspaceRuntime::new(beta.workspace_id, beta.session_ids.clone());
    let _ = previous.handle_key(Key::Down);

    remember_workspace_session_focus(&mut deck, previous.state());
    let _ = previous.handle_key(Key::Down);
    remember_workspace_session_focus(&mut deck, previous.state());
    assert_eq!(
        deck.focused_session_for_path(&beta.workspace.path),
        Some(remembered),
        "the transient new-session row does not replace the last session focus"
    );

    let mut restored = WorkspaceRuntime::new(beta.workspace_id, beta.session_ids.clone());
    restore_workspace_session_focus(&deck, &beta.workspace.path, &mut restored);
    assert_eq!(
        restored.state().selected(),
        crate::usecase::application::controller::Selection::Target(Target::Session(remembered))
    );
    assert_eq!(restored.state().route(), Route::Home(HomeMode::Switch));

    deck.schedule_closeup(beta.workspace.path.clone());
    let _ = restored.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
        beta.session_lifecycles.clone(),
    )));
    restore_workspace_closeup(&mut deck, &beta.workspace.path, &mut restored);
    assert_eq!(restored.state().active(), Some(remembered));
    assert_eq!(restored.state().route(), Route::Home(HomeMode::Closeup));

    let transition = strip_ansi(
        &cached_workspace_switch_frame(
            &deck,
            &beta.workspace.path,
            24,
            80,
            0,
            "Opening workspace…",
            false,
        )
        .unwrap()
        .join("\n"),
    );
    assert!(transition.contains("> remembered"), "{transition}");
    assert!(!transition.contains("> first"), "{transition}");
}

#[test]
fn workspace_switch_progress_appears_only_after_the_grace_or_cancellation() {
    assert!(!workspace_loading_visible(
        true,
        false,
        WORKSPACE_SWITCH_LOADING_GRACE
            .checked_sub(std::time::Duration::from_millis(1))
            .expect("the loading grace exceeds one millisecond"),
    ));
    assert!(workspace_loading_visible(
        true,
        false,
        WORKSPACE_SWITCH_LOADING_GRACE,
    ));
    assert!(workspace_loading_visible(
        true,
        true,
        std::time::Duration::ZERO,
    ));
    assert!(workspace_loading_visible(
        false,
        false,
        std::time::Duration::ZERO,
    ));
}

#[test]
fn prepared_existing_tab_uses_the_cached_switch_surface() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
    let mut term = ResponsiveLoadingTerminal::default();
    let mut loader = FakeLoader {
        operation_mode: FakeOperationMode::Background,
        open_delay: std::time::Duration::from_millis(8),
        ..FakeLoader::default()
    };
    let mut loader_port: Option<&mut dyn WorkspaceLoader> = Some(&mut loader);

    let prepared = prepare_deck_workspace(
        &mut term,
        &mut loader_port,
        &mut deck,
        &beta.workspace.path,
        "Opening workspace…",
    );

    assert!(prepared.is_some());
    assert!(!term.frames.is_empty());
    assert!(
        term.frames
            .iter()
            .all(|frame| frame.join("\n").contains("beta-session"))
    );
}

#[test]
fn cancelled_switch_never_replaces_the_deck_with_a_full_screen_loader() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let mut deck = WorkspaceDeck::from_snapshots(&[alpha, beta.clone()]).unwrap();
    let mut term = ResponsiveLoadingTerminal {
        wait_keys: VecDeque::from([Key::Escape]),
        ..ResponsiveLoadingTerminal::default()
    };
    let mut loader = FakeLoader {
        operation_mode: FakeOperationMode::Background,
        open_delay: std::time::Duration::from_millis(8),
        ..FakeLoader::default()
    };
    let mut loader_port: Option<&mut dyn WorkspaceLoader> = Some(&mut loader);

    assert!(
        prepare_deck_workspace(
            &mut term,
            &mut loader_port,
            &mut deck,
            &beta.workspace.path,
            "Opening workspace…",
        )
        .is_none()
    );

    assert!(term.frames.iter().all(|frame| {
        let frame = frame.join("\n");
        frame.contains("1 alpha")
            && frame.contains("2 beta")
            && (frame.contains("alpha-session") || frame.contains("beta-session"))
    }));
}

#[test]
fn a_panicking_background_operation_is_reported_as_io_failure() {
    let mut term = ResponsiveLoadingTerminal::default();
    let error = run_workspace_loading(
        &mut term,
        "Saving settings…",
        false,
        || -> io::Result<()> { panic!("injected worker panic") },
    )
    .expect_err("worker panic is mapped to a stable error");

    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(
        error.to_string(),
        "background operation stopped unexpectedly"
    );
}

#[test]
fn production_workspace_ports_open_refresh_and_activate_on_the_worker_path() {
    let mut term = ResponsiveLoadingTerminal::default();
    let mut loader = FakeLoader {
        operation_mode: FakeOperationMode::Background,
        open_snapshot: FakeOpenSnapshot::Empty,
        ..FakeLoader::default()
    };

    let snapshot = open_workspace_responsive(
        &mut term,
        &mut loader,
        Path::new("/tmp/background"),
        "Opening workspace…",
    )
    .expect("background open succeeds");
    activate_workspace_responsive(
        &mut term,
        &mut loader,
        &snapshot.workspace.path,
        "Activating workspace…",
    )
    .expect("background activation succeeds");

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/background")]);
    assert_eq!(loader.refreshed, vec![PathBuf::from("/tmp/background")]);

    let paths = vec![PathBuf::from("/tmp/one"), PathBuf::from("/tmp/two")];
    let (_, active, _) = prepare_workspace_deck(&mut term, &mut loader, &paths)
        .expect("background deck preparation succeeds");
    assert_eq!(active.workspace.path, PathBuf::from("/tmp/one"));

    let mut failing_loader = FakeLoader {
        activate_error: Some("activation failed"),
        ..FakeLoader::default()
    };
    let error = prepare_workspace_deck(
        &mut term,
        &mut failing_loader,
        &[PathBuf::from("/tmp/fail")],
    )
    .expect_err("activation failure is propagated");
    assert_eq!(error.to_string(), "activation failed");
}

#[test]
fn cancelling_recent_and_open_list_restores_the_originating_screen() {
    let cases = [
        (
            vec![Key::Char('1'), Key::Quit],
            Vec::new(),
            vec![recent("recent")],
            "Menu",
        ),
        (
            vec![Key::Char('o'), Key::Enter, Key::Quit],
            vec![ws("open")],
            Vec::new(),
            "Open Workspace",
        ),
    ];

    for (keys, workspaces, recent, originating_screen) in cases {
        let mut term = ResponsiveLoadingTerminal {
            keys: keys.into(),
            wait_keys: VecDeque::from([Key::Escape]),
            ..ResponsiveLoadingTerminal::default()
        };
        let mut loader = FakeLoader {
            operation_mode: FakeOperationMode::Background,
            open_delay: std::time::Duration::from_millis(20),
            ..FakeLoader::default()
        };
        let mut settings = RecordingSettingsPort::default();
        let mut sessions = UnavailableSessionCommandPortFactory;

        assert_eq!(
            run_with_settings(
                &mut term,
                workspaces,
                recent,
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );
        let frames = term
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains("Workspace opening was cancelled.")),
            "cancel notice was absent from {originating_screen}: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.contains(originating_screen)),
            "originating screen was absent: {frames:?}"
        );
    }
}

#[test]
fn run_quits_from_welcome_and_handles_menu_navigation() {
    for keys in [
        vec![Key::Char('q'), Key::Enter],
        vec![Key::Quit],
        vec![Key::Escape],
        vec![Key::Down, Key::Down, Key::Up, Key::Quit],
        vec![Key::Down, Key::Down, Key::Down, Key::Enter],
    ] {
        let mut term = FakeTerminal::with_keys(&keys);
        assert_eq!(
            run(
                &mut term,
                Vec::new(),
                Vec::new(),
                now(),
                &mut FakeLoader::default(),
            )
            .unwrap(),
            Exit::Quit
        );
        assert!(term.frames[0].join("\n").contains("Menu"));
    }
}

#[test]
fn startup_splash_draws_and_paces_every_frame_without_reading_input() {
    let mut term = FakeTerminal::default();

    play_startup_splash(&mut term).unwrap();

    assert_eq!(
        term.frames.len(),
        crate::presentation::views::splash::FRAMES
    );
    assert_eq!(term.waits.len(), crate::presentation::views::splash::FRAMES);
    assert!(
        term.waits
            .iter()
            .all(|wait| *wait == crate::presentation::views::splash::ANIM_TICK)
    );
    assert!(term.keys.is_empty());
}

#[test]
fn run_ignores_unknown_welcome_keys() {
    let keys = [
        Key::Char('z'),
        Key::Left,
        Key::Right,
        Key::Backspace,
        Key::Other,
        Key::Char('q'),
        Key::Enter,
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    // None of these keys changes Welcome, so the gate draws the menu once
    // and every ignored key costs nothing (#554).
    assert_eq!(term.frames.len(), 1);
    assert!(
        term.frames
            .iter()
            .all(|frame| frame.join("\n").contains("Menu"))
    );
}

#[test]
fn entry_help_is_contextual_and_exclusively_owns_input_until_closed() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Help,
        Key::Down,
        // This would open the workspace list if Help did not own input.
        Key::Char('o'),
        Key::Escape,
        Key::Char('q'),
    ]);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();

    assert_eq!(term.frames.len(), 4);
    assert!(
        term.frames[1]
            .join("\n")
            .contains("Keyboard help · Welcome")
    );
    assert!(term.frames[1].join("\n").contains("open Recent card"));
    assert!(
        term.frames[2]
            .join("\n")
            .contains("Keyboard help · Welcome")
    );
    assert!(term.frames[3].join("\n").contains("Menu"));
    assert!(
        term.frames
            .iter()
            .all(|frame| !frame.join("\n").contains("Open Workspace"))
    );
}

#[test]
fn entry_help_resolves_every_entry_surface_and_config_submode() {
    use super::Screen;
    use crate::presentation::views::key_help::Context as HelpContext;

    let mut settings = DefaultSettingsPort;
    let config = Config::load(&mut settings);
    let mut open = Open::new(vec![ws("atlas")]);

    for (screen, expected) in [
        (Screen::Welcome, HelpContext::Welcome),
        (Screen::Open, HelpContext::Open),
        (Screen::New, HelpContext::New),
        (Screen::Config, HelpContext::Config),
    ] {
        assert_eq!(
            super::entry_help_context(screen, &open, &config, false),
            expected
        );
    }
    assert_eq!(
        super::entry_help_context(Screen::Welcome, &open, &config, true),
        HelpContext::MissingWorkspace
    );

    open.request_unregister();
    assert_eq!(
        super::entry_help_context(Screen::Open, &open, &config, false),
        HelpContext::OpenUnregister
    );
    open.cancel_unregister();
    open.request_cleanup();
    assert_eq!(
        super::entry_help_context(Screen::Open, &open, &config, false),
        HelpContext::OpenCleanup
    );

    let mut team = Config::load(&mut settings);
    for _ in 0..7 {
        let _ = step_config(&mut team, Key::Down, &mut settings);
    }
    let _ = step_config(&mut team, Key::Enter, &mut settings);
    assert_eq!(super::config_help_context(&team), HelpContext::TeamPicker);

    let mut environment = Config::load(&mut settings);
    for _ in 0..4 {
        let _ = step_config(&mut environment, Key::Down, &mut settings);
    }
    let _ = step_config(&mut environment, Key::Enter, &mut settings);
    assert_eq!(
        super::config_help_context(&environment),
        HelpContext::EnvironmentEditor
    );

    let mut setup =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut setup, Key::Down, &mut settings);
    }
    let _ = step_config(&mut setup, Key::Enter, &mut settings);
    assert_eq!(
        super::config_help_context(&setup),
        HelpContext::SessionSetupEditor
    );
}

#[test]
fn welcome_action_maps_every_destination() {
    assert!(matches!(
        welcome_action(MenuAction::Quit),
        WelcomeStep::Quit
    ));
    assert!(matches!(
        welcome_action(MenuAction::Open),
        WelcomeStep::OpenList
    ));
    assert!(matches!(
        welcome_action(MenuAction::OpenRecent(2)),
        WelcomeStep::OpenRecent(2)
    ));
    assert!(matches!(
        welcome_action(MenuAction::New),
        WelcomeStep::NewForm
    ));
    assert!(matches!(
        welcome_action(MenuAction::Config),
        WelcomeStep::ConfigScreen
    ));
}

#[test]
fn config_can_be_opened_from_welcome_or_used_as_the_start() {
    let mut from_welcome =
        FakeTerminal::with_keys(&[Key::Char('c'), Key::Escape, Key::Char('q'), Key::Enter]);
    run(
        &mut from_welcome,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(from_welcome.frames[0].join("\n").contains("Menu"));
    assert!(from_welcome.frames[1].join("\n").contains("Config"));
    assert!(from_welcome.frames[2].join("\n").contains("Menu"));

    let mut direct = FakeTerminal::with_keys(&[Key::Char('x'), Key::Quit]);
    run_from_start(
        &mut direct,
        Vec::new(),
        Vec::new(),
        now(),
        Start::Config,
        &mut FakeLoader::default(),
    )
    .unwrap();
    // `x` changes nothing on Config, so only the entry frame is drawn.
    assert_eq!(direct.frames.len(), 1);
    assert!(
        direct
            .frames
            .iter()
            .all(|frame| frame.join("\n").contains("Config"))
    );
}

#[test]
fn step_config_maps_back_quit_and_stay() {
    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    assert!(matches!(
        step_config(&mut config, Key::Escape, &mut settings),
        ConfigStep::Back
    ));
    assert!(matches!(
        step_config(&mut config, Key::Quit, &mut settings),
        ConfigStep::Quit
    ));
    assert!(matches!(
        step_config(&mut config, Key::Char('x'), &mut settings),
        ConfigStep::Stay
    ));
    assert!(matches!(
        step_config(&mut config, Key::Tab, &mut settings),
        ConfigStep::Stay
    ));
    for key in [
        Key::Up,
        Key::Down,
        Key::Char('j'),
        Key::Left,
        Key::Right,
        Key::CtrlQ,
    ] {
        let _ = step_config(&mut config, key, &mut settings);
    }
}

#[test]
fn step_config_opens_applies_and_cancels_the_team_picker() {
    use crate::presentation::views::config::Field as ConfigField;
    use usagi_core::domain::settings::TeamTemplate;

    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    for _ in 0..7 {
        step_config(&mut config, Key::Down, &mut settings);
    }
    assert_eq!(config.field(), ConfigField::TeamTemplate);
    step_config(&mut config, Key::Right, &mut settings);
    assert_eq!(config.settings().team_template, TeamTemplate::None);

    step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_selecting_team());
    step_config(&mut config, Key::Other, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Up, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Enter, &mut settings);
    assert!(!config.is_selecting_team());
    assert_eq!(config.settings().team_template, TeamTemplate::Flat);

    step_config(&mut config, Key::Enter, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Up, &mut settings);
    step_config(&mut config, Key::Left, &mut settings);
    step_config(&mut config, Key::Tab, &mut settings);
    step_config(&mut config, Key::Escape, &mut settings);
    assert!(!config.is_selecting_team());
    assert_eq!(config.settings().team_template, TeamTemplate::Flat);
}

#[test]
fn step_config_routes_input_to_the_global_environment_editor() {
    use crate::presentation::views::config::Field as ConfigField;

    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    assert_eq!(config.field(), ConfigField::Environment);
    step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_editing_environment());
    step_config(&mut config, Key::Char('C'), &mut settings);
    step_config(&mut config, Key::Paste("=3xy".to_owned()), &mut settings);
    step_config(&mut config, Key::Backspace, &mut settings);
    step_config(&mut config, Key::Left, &mut settings);
    step_config(&mut config, Key::Delete, &mut settings);
    step_config(&mut config, Key::End, &mut settings);
    step_config(&mut config, Key::Enter, &mut settings);
    step_config(
        &mut config,
        Key::Paste("A=1\r\nB=2".to_owned()),
        &mut settings,
    );
    step_config(&mut config, Key::Up, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Home, &mut settings);
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::LineEnd, &mut settings);
    step_config(&mut config, Key::LineStart, &mut settings);
    step_config(&mut config, Key::Tab, &mut settings);
    assert!(!config.is_environment_save_focused());
    step_config(&mut config, Key::Other, &mut settings);
    step_config(
        &mut config,
        Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![19],
        },
        &mut settings,
    );
    assert!(!config.is_editing_environment());
    assert_eq!(config.settings().env["A"], "1");
    assert_eq!(config.settings().env["B"], "2");
    assert_eq!(config.settings().env["C"], "3");

    step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_editing_environment());
    step_config(&mut config, Key::Escape, &mut settings);
    assert!(!config.is_editing_environment());
}

#[test]
fn step_config_saves_the_workspace_environment_from_its_save_action() {
    let mut settings = DefaultSettingsPort;
    let mut config =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Enter, &mut settings);
    step_config(&mut config, Key::Char('A'), &mut settings);
    step_config(&mut config, Key::Paste("=1".to_owned()), &mut settings);
    step_config(&mut config, Key::Tab, &mut settings);
    assert!(config.is_environment_save_focused());
    step_config(&mut config, Key::Enter, &mut settings);

    assert!(!config.is_editing_environment());
    assert_eq!(config.settings().env["A"], "1");
}

#[test]
fn step_config_saves_only_from_the_dirty_save_row() {
    let mut settings = DefaultSettingsPort;
    let mut config = Config::load(&mut settings);
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Stay
    ));
    step_config(&mut config, Key::Right, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    step_config(&mut config, Key::Down, &mut settings);
    // Enter on the dirty Save row begins the save flow (loading).
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Save
    ));
    // A second Enter while Saving is a no-op, so it stays on the screen.
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Stay
    ));
}

/// Settings port that records saves and can be told to fail, for the Config
/// save screen-graph tests.
#[derive(Default)]
struct RecordingSettingsPort {
    saves: usize,
    environment_saves: usize,
    setup_saves: usize,
    setup_commands: Vec<String>,
    background: bool,
    fail_save: bool,
}

#[derive(Default)]
struct WorkspaceBindingSettingsPort {
    selected: Vec<PathBuf>,
    saves: Vec<(SettingsScope, Settings)>,
    refuse: Option<PathBuf>,
}

impl SettingsPort for WorkspaceBindingSettingsPort {
    fn select_workspace(&mut self, workspace_root: &Path) -> io::Result<()> {
        self.selected.push(workspace_root.to_path_buf());
        if self.refuse.as_deref() == Some(workspace_root) {
            return Err(io::Error::other("workspace settings are unreadable"));
        }
        Ok(())
    }

    fn read(
        &mut self,
        _scope: usagi_core::usecase::settings::SettingsScope,
    ) -> io::Result<usagi_core::domain::settings::Settings> {
        Ok(usagi_core::domain::settings::Settings {
            modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode::Prompt,
            ..usagi_core::domain::settings::Settings::default()
        })
    }

    fn save(
        &mut self,
        scope: usagi_core::usecase::settings::SettingsScope,
        settings: &usagi_core::domain::settings::Settings,
    ) -> io::Result<()> {
        self.saves.push((scope, settings.clone()));
        Ok(())
    }
}

#[test]
fn overview_config_saves_the_current_workspace_and_returns_to_home() {
    let mut keys = vec![Key::Char('o'), Key::Enter, Key::Char(':')];
    keys.extend("config".chars().map(Key::Char));
    keys.extend([
        Key::Enter,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Right,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            vec![ws("project")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(settings.selected, vec![PathBuf::from("/tmp/project")]);
    assert_eq!(settings.saves.len(), 1);
    assert_eq!(settings.saves[0].0, SettingsScope::Workspace);
    assert!(!settings.saves[0].1.issue_enabled);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    let config = frames
        .iter()
        .position(|frame| {
            frame.contains("Config") && frame.contains("Agent") && !frame.contains("Scope:")
        })
        .expect("workspace Config is rendered");
    assert!(frames[config].contains("project"));
    assert!(!frames[config].contains("Overview"));
    let done = frames
        .iter()
        .position(|frame| frame.contains("Config") && frame.contains("[ done ]"))
        .expect("workspace Config shows done before closing");
    let returned_home = frames
        .iter()
        .skip(done + 1)
        .any(|frame| frame.contains("project") && !frame.contains("Config"));
    assert!(config < done && returned_home);
    assert_eq!(term.waits, config_save_waits(true));
}

#[test]
fn screen_graph_binds_settings_for_open_recent_and_new_entries() {
    let cases = [
        (
            vec![Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')],
            vec![ws("open")],
            Vec::new(),
            PathBuf::from("/tmp/open"),
        ),
        (
            vec![Key::Char('1'), Key::CtrlQ, Key::Char('y')],
            Vec::new(),
            vec![recent("recent")],
            PathBuf::from("/tmp/recent"),
        ),
        (
            vec![
                Key::Char('e'),
                Key::Right,
                Key::Down,
                Key::Char('x'),
                Key::Enter,
                Key::CtrlQ,
                Key::Char('y'),
            ],
            Vec::new(),
            Vec::new(),
            PathBuf::from("/tmp/x"),
        ),
    ];

    for (keys, workspaces, recent, expected) in cases {
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut settings = WorkspaceBindingSettingsPort::default();
        let mut sessions = UnavailableSessionCommandPortFactory;
        assert_eq!(
            run_with_settings(
                &mut term,
                workspaces,
                recent,
                now(),
                Start::Welcome,
                &mut loader,
                &mut settings,
                &mut sessions,
            )
            .unwrap(),
            Exit::Quit
        );
        assert_eq!(settings.selected, vec![expected]);
    }
}

impl SettingsPort for RecordingSettingsPort {
    fn background_operations(&self) -> bool {
        self.background
    }

    fn read(
        &mut self,
        _scope: usagi_core::usecase::settings::SettingsScope,
    ) -> io::Result<usagi_core::domain::settings::Settings> {
        Ok(usagi_core::domain::settings::Settings::default())
    }

    fn save(
        &mut self,
        _scope: usagi_core::usecase::settings::SettingsScope,
        _settings: &usagi_core::domain::settings::Settings,
    ) -> io::Result<()> {
        if self.fail_save {
            return Err(io::Error::other("disk unavailable"));
        }
        self.saves += 1;
        Ok(())
    }

    fn save_environment(
        &mut self,
        _scope: SettingsScope,
        _environment: &usagi_core::domain::settings::EnvBindings,
    ) -> io::Result<()> {
        self.environment_saves += 1;
        Ok(())
    }

    fn read_workspace_setup_commands(&mut self) -> io::Result<Vec<String>> {
        Ok(self.setup_commands.clone())
    }

    fn save_workspace_setup_commands(&mut self, commands: &[String]) -> io::Result<()> {
        self.setup_saves += 1;
        self.setup_commands = commands.to_vec();
        Ok(())
    }
}

#[test]
fn step_config_routes_workspace_session_setup_editor_input_and_cancel() {
    let mut settings = RecordingSettingsPort::default();
    let mut config =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut config, Key::Down, &mut settings);
    }
    let _ = step_config(&mut config, Key::Enter, &mut settings);
    assert!(config.is_editing_setup_commands());
    for key in [
        Key::Char('x'),
        Key::Paste("y".to_owned()),
        Key::Backspace,
        Key::Left,
        Key::Delete,
        Key::Paste("one\r\ntwo".to_owned()),
        Key::Up,
        Key::Down,
        Key::Home,
        Key::Right,
        Key::End,
        Key::LineStart,
        Key::LineEnd,
        Key::Enter,
        Key::Paste("three".to_owned()),
        Key::Tab,
        Key::Other,
        Key::Tab,
        Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![19],
        },
    ] {
        let _ = step_config(&mut config, key, &mut settings);
    }
    let _ = step_config(&mut config, Key::Escape, &mut settings);
    assert!(!config.is_editing_setup_commands());
    assert_eq!(settings.setup_saves, 0);

    let _ = step_config(&mut config, Key::Enter, &mut settings);
    let _ = step_config(
        &mut config,
        Key::Paste("cargo fetch\ncargo test".to_owned()),
        &mut settings,
    );
    let _ = step_config(&mut config, Key::Tab, &mut settings);
    let _ = step_config(&mut config, Key::Enter, &mut settings);
    assert!(!config.is_editing_setup_commands());
    assert_eq!(settings.setup_commands, ["cargo fetch", "cargo test"]);
}

#[test]
fn production_config_and_source_writes_use_the_responsive_worker_path() {
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut config = Config::load(&mut settings);
    let _ = step_config(&mut config, Key::Right, &mut settings);
    for _ in 0..11 {
        let _ = step_config(&mut config, Key::Down, &mut settings);
    }
    assert!(matches!(
        step_config(&mut config, Key::Enter, &mut settings),
        ConfigStep::Save
    ));
    let mut term = ResponsiveLoadingTerminal::default();
    assert!(
        save_config_responsive(&mut term, &mut config, &mut settings, None)
            .expect("responsive settings save")
    );

    let mut environment =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    let _ = step_config(&mut environment, Key::Down, &mut settings);
    let _ = step_config(&mut environment, Key::Enter, &mut settings);
    let _ = step_config(
        &mut environment,
        Key::Paste("A=1".to_owned()),
        &mut settings,
    );
    let _ = step_config(&mut environment, Key::Tab, &mut settings);
    assert!(matches!(
        step_config(&mut environment, Key::Enter, &mut settings),
        ConfigStep::SaveSource
    ));
    assert!(save_config_source_responsive(
        &mut term,
        &mut environment,
        &mut settings,
    ));

    let mut setup =
        Config::load_workspace_with_available_models(&mut settings, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut setup, Key::Down, &mut settings);
    }
    let _ = step_config(&mut setup, Key::Enter, &mut settings);
    let _ = step_config(
        &mut setup,
        Key::Paste("cargo fetch\ncargo test".to_owned()),
        &mut settings,
    );
    let _ = step_config(&mut setup, Key::Tab, &mut settings);
    assert!(matches!(
        step_config(&mut setup, Key::Enter, &mut settings),
        ConfigStep::SaveSource
    ));
    assert!(save_config_source_responsive(
        &mut term,
        &mut setup,
        &mut settings,
    ));

    assert_eq!(settings.saves, 1);
    assert_eq!(settings.environment_saves, 1);
    assert_eq!(settings.setup_saves, 1);
    assert_eq!(settings.setup_commands, ["cargo fetch", "cargo test"]);
    assert_eq!(environment.settings().env["A"], "1");
    let painted = term.frames.iter().flatten().cloned().collect::<String>();
    assert!(painted.contains("Saving settings…"));
    assert!(painted.contains("Saving environment…"));
    assert!(painted.contains("Saving session setup…"));
}

#[test]
fn responsive_save_helpers_cover_empty_and_inline_environment_requests() {
    let mut background = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut clean = Config::load(&mut background);
    let mut term = ResponsiveLoadingTerminal::default();
    assert!(!save_config_responsive(&mut term, &mut clean, &mut background, None).unwrap());
    assert!(!save_environment_responsive(
        &mut term,
        &mut clean,
        &mut background
    ));
    assert!(!save_setup_commands_responsive(
        &mut term,
        &mut clean,
        &mut background
    ));

    let mut inline = RecordingSettingsPort::default();
    let mut environment = Config::load(&mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Down, &mut inline);
    let _ = step_config(&mut environment, Key::Enter, &mut inline);
    let _ = step_config(
        &mut environment,
        Key::Paste("INLINE=1".to_owned()),
        &mut inline,
    );
    let _ = step_config(&mut environment, Key::Tab, &mut inline);
    assert!(save_environment_responsive(
        &mut term,
        &mut environment,
        &mut inline
    ));
    assert_eq!(inline.environment_saves, 1);

    let mut setup =
        Config::load_workspace_with_available_models(&mut inline, AvailableAgentModels::all());
    for _ in 0..3 {
        let _ = step_config(&mut setup, Key::Down, &mut inline);
    }
    let _ = step_config(&mut setup, Key::Enter, &mut inline);
    let _ = step_config(&mut setup, Key::Paste("cargo test".to_owned()), &mut inline);
    assert!(save_setup_commands_responsive(
        &mut term,
        &mut setup,
        &mut inline,
    ));
    assert_eq!(inline.setup_commands, ["cargo test"]);
}

#[test]
fn background_environment_shortcut_reaches_both_config_surfaces() {
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut config = Config::load(&mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Down, &mut settings);
    let _ = step_config(&mut config, Key::Enter, &mut settings);
    let save = Key::Management {
        action: AppKey::SaveRoles,
        passthrough: vec![0x13],
    };
    assert!(matches!(
        step_config(&mut config, save.clone(), &mut settings),
        ConfigStep::SaveSource
    ));
    assert!(matches!(
        step_workspace_config(&mut config, save, &mut settings),
        WorkspaceConfigStep::SaveSource
    ));
}

#[test]
fn workspace_config_dispatches_background_environment_save() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut term = FakeTerminal::with_keys(&[
        Key::Down,
        Key::Enter,
        Key::Paste("WORKSPACE=1".to_owned()),
        Key::Tab,
        Key::Enter,
        Key::Escape,
    ]);

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    assert_eq!(settings.environment_saves, 1);
}

#[test]
fn workspace_config_dispatches_background_session_setup_save() {
    let base = vec!["home".to_owned(); 28];
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut term = FakeTerminal::with_keys(&[
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::Paste("cargo fetch\ncargo test".to_owned()),
        Key::Tab,
        Key::Enter,
        Key::Escape,
    ]);

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    assert_eq!(settings.setup_saves, 1);
    assert_eq!(settings.setup_commands, ["cargo fetch", "cargo test"]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Session setup"))
    );
}

#[test]
fn full_config_dispatches_background_environment_save() {
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut loader = FakeLoader::default();
    let mut sessions = UnavailableSessionCommandPortFactory;
    let mut term = FakeTerminal::with_keys(&[
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::Paste("GLOBAL=1".to_owned()),
        Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![0x13],
        },
        Key::Escape,
        Key::Quit,
    ]);

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Config,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(settings.environment_saves, 1);
}

// Focus the dirty Save row from Global Config: cycle the theme, then step down to
// Save (Theme → Icons → Modal mode → Terminal PTYs → Environment → Agent model →
// Workflow → Team → Issue → Memory → PR → Save).
const CONFIG_SAVE_KEYS: [Key; 13] = [
    Key::Right,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Enter,
];

// Workspace Config starts on Agent and contains Agent → env → Base branch →
// Session setup → Workflow → Team → Issue → Memory → Save.
const WORKSPACE_CONFIG_SAVE_KEYS: [Key; 10] = [
    Key::Right,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Down,
    Key::Enter,
];

fn config_save_waits(done: bool) -> Vec<std::time::Duration> {
    let mut waits = vec![
        crate::presentation::views::config::SAVE_WAVE_TICK;
        crate::presentation::views::config::SAVE_WAVE_FRAMES - 1
    ];
    if done {
        waits.push(crate::presentation::views::config::DONE_DISPLAY);
    }
    waits
}

#[test]
fn workspace_config_handles_back_and_failed_save_without_leaving_drafts() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort::default();
    let mut back = FakeTerminal::with_keys(&[Key::Escape]);
    run_workspace_config(
        &mut back,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    let keys = WORKSPACE_CONFIG_SAVE_KEYS
        .iter()
        .cloned()
        .chain(std::iter::once(Key::Escape))
        .collect::<Vec<_>>();
    let mut failed = FakeTerminal::with_keys(&keys);
    let mut failing_settings = RecordingSettingsPort {
        fail_save: true,
        ..RecordingSettingsPort::default()
    };
    run_workspace_config(
        &mut failed,
        &mut failing_settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();
    assert_eq!(failed.waits, config_save_waits(false));
    assert!(
        failed
            .frames
            .iter()
            .any(|frame| frame.join("\n").contains("Save failed"))
    );
}

#[test]
fn background_workspace_config_save_keeps_one_modal_visible_until_done() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort {
        background: true,
        ..RecordingSettingsPort::default()
    };
    let mut term = ResponsiveLoadingTerminal {
        keys: VecDeque::from(WORKSPACE_CONFIG_SAVE_KEYS),
        ..ResponsiveLoadingTerminal::default()
    };

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    assert_eq!(settings.saves, 1);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(frames.iter().all(|frame| frame.contains("Config")));
    assert!(
        frames
            .iter()
            .all(|frame| !frame.contains("Saving settings…"))
    );
    assert!(frames.iter().any(|frame| frame.contains("[ done ]")));
}

#[test]
fn workspace_config_swallows_quit_keys_until_escape() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort::default();
    let mut term = FakeTerminal::with_keys(&[Key::Quit, Key::CtrlQ, Key::Char('q'), Key::Escape]);

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    assert_eq!(term.frames.len(), 4);
    assert!(
        term.frames
            .iter()
            .all(|frame| frame.join("\n").contains("Config"))
    );
}

#[test]
fn workspace_config_help_toggles_and_exclusively_owns_input() {
    let base = vec!["home".to_owned(); 24];
    let mut settings = RecordingSettingsPort::default();
    let mut term = FakeTerminal::with_keys(&[
        Key::Help,
        Key::Quit,
        Key::Help,
        Key::Help,
        Key::Escape,
        Key::Escape,
    ]);

    run_workspace_config(
        &mut term,
        &mut settings,
        AvailableAgentModels::all(),
        &[],
        &base,
    )
    .unwrap();

    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 6);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.contains("Keyboard help · Config"))
            .count(),
        3
    );
    assert!(!frames.last().unwrap().contains("Keyboard help"));
}

#[test]
fn config_save_waves_then_shows_done_and_returns_home_on_its_own() {
    let keys: Vec<Key> = CONFIG_SAVE_KEYS
        .iter()
        .cloned()
        .chain(std::iter::once(Key::Quit)) // now on Welcome; quit to end the loop
        .collect();
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = RecordingSettingsPort::default();
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Config,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    // Exactly one write, one complete wave, and one confirmation dwell —
    // the screen returned home on the timer, with no extra key press.
    assert_eq!(settings.saves, 1);
    assert_eq!(term.waits, config_save_waits(true));

    // Frames appear in order: an animated Save caption, then `done`, then
    // the Welcome `Menu` reached without a key press.
    let joined: Vec<String> = term.frames.iter().map(|frame| frame.join("\n")).collect();
    let done = joined
        .iter()
        .position(|frame| frame.contains("[ done ]"))
        .expect("a done confirmation frame is drawn");
    let wave = &term.frames[done - crate::presentation::views::config::SAVE_WAVE_FRAMES..done];
    assert!(wave.iter().all(|frame| {
        frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
            .contains("[ Save ]")
    }));
    assert!(wave.windows(2).all(|frames| frames[0] != frames[1]));
    let menu = joined
        .iter()
        .rposition(|frame| frame.contains("Menu"))
        .expect("the Welcome menu is drawn after returning home");
    assert!(done < menu);
}

#[test]
fn config_save_failure_stays_on_the_screen_without_dwelling_or_returning() {
    let keys: Vec<Key> = CONFIG_SAVE_KEYS
        .iter()
        .cloned()
        .chain([Key::Escape, Key::Quit]) // still on Config; Esc back, then quit
        .collect();
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = RecordingSettingsPort {
        fail_save: true,
        ..RecordingSettingsPort::default()
    };
    let mut sessions = UnavailableSessionCommandPortFactory;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Config,
            &mut loader,
            &mut settings,
            &mut sessions,
        )
        .unwrap(),
        Exit::Quit
    );

    // A failed write still animates while pending, but neither dwells on
    // `done` nor auto-returns.
    assert_eq!(settings.saves, 0);
    assert_eq!(term.waits, config_save_waits(false));

    let joined: Vec<String> = term.frames.iter().map(|frame| frame.join("\n")).collect();
    // The error is surfaced on the Config screen and no `done` confirmation
    // is ever shown.
    assert!(joined.iter().any(|frame| frame.contains("Save failed")));
    assert!(joined.iter().all(|frame| !frame.contains("[ done ]")));
}

#[test]
fn new_form_opens_edits_and_returns_to_welcome() {
    let keys = [
        Key::Char('e'),
        Key::Down,
        Key::Char('a'),
        Key::Backspace,
        Key::Escape,
        Key::Char('q'),
        Key::Enter,
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[0].join("\n").contains("Menu"));
    assert!(
        term.frames[1..5]
            .iter()
            .all(|frame| frame.join("\n").contains("New Project"))
    );
    assert!(term.frames[5].join("\n").contains("Menu"));
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
fn step_new_handles_every_edit_and_exit_key() {
    let mut form = New::default();
    assert!(matches!(step_new(&mut form, Key::Down), NewStep::Stay));
    assert_eq!(form.focus(), Field::Url);
    assert!(matches!(step_new(&mut form, Key::Up), NewStep::Stay));
    assert_eq!(form.focus(), Field::Mode);
    step_new(&mut form, Key::Right);
    assert_eq!(form.mode(), Mode::Existing);
    step_new(&mut form, Key::Left);
    assert_eq!(form.mode(), Mode::Clone);
    step_new(&mut form, Key::Down);
    step_new(&mut form, Key::Char('a'));
    step_new(&mut form, Key::Char('b'));
    step_new(&mut form, Key::Left);
    step_new(&mut form, Key::Right);
    step_new(&mut form, Key::Backspace);
    for key in [
        Key::Home,
        Key::End,
        Key::LineStart,
        Key::LineEnd,
        Key::SelectLeft,
        Key::SelectRight,
        Key::SelectHome,
        Key::SelectEnd,
        Key::Delete,
        Key::Tab,
        Key::CtrlD,
        Key::Live(LiveTerminalAction::NextTab),
        Key::Click { column: 0, row: 0 },
        Key::Passthrough(Vec::new()),
    ] {
        let _ = step_new(&mut form, key);
    }
    assert_eq!(form.url(), "a");
    // Enter with a still-incomplete Clone form (no Location) validates,
    // surfaces the field error as a notice, and stays on the form.
    assert!(matches!(step_new(&mut form, Key::Enter), NewStep::Stay));
    assert_eq!(form.notice(), Some("clone location is required"));
    assert!(matches!(step_new(&mut form, Key::Other), NewStep::Stay));
    assert!(matches!(step_new(&mut form, Key::Escape), NewStep::Back));
    assert!(matches!(step_new(&mut form, Key::Quit), NewStep::Quit));
    assert!(matches!(step_new(&mut form, Key::CtrlQ), NewStep::Quit));
}

#[test]
fn step_new_paste_inserts_the_pasted_text_into_the_focused_field() {
    let mut form = New::default();
    step_new(&mut form, Key::Down); // focus the Url field
    assert!(matches!(
        step_new(
            &mut form,
            Key::Paste("https://example.com/repo.git".to_owned()),
        ),
        NewStep::Stay
    ));
    assert_eq!(form.url(), "https://example.com/repo.git");
}

#[test]
fn step_open_paste_appends_its_text_to_the_filter() {
    let mut open = Open::new(vec![ws("alpha")]);
    assert!(matches!(
        step_open(&mut open, Key::Paste("alp".to_owned())),
        OpenStep::Stay
    ));
    assert_eq!(open.filter(), "alp");
}

#[test]
fn step_welcome_ignores_a_bracketed_paste() {
    let mut welcome = super::Welcome::new(Vec::new());
    assert!(matches!(
        super::step_welcome(&mut welcome, Key::Paste("x".to_owned())),
        super::WelcomeStep::Stay
    ));
}

#[test]
fn step_new_enter_creates_once_every_required_field_is_present() {
    let mut form = New::default();
    step_new(&mut form, Key::Down); // Url
    for ch in "https://example.com/owner/repo.git".chars() {
        step_new(&mut form, Key::Char(ch));
    }
    step_new(&mut form, Key::Down); // Location
    for ch in "/projects".chars() {
        step_new(&mut form, Key::Char(ch));
    }
    // Directory は URL から導出済み。Enter で検証済みの Create を返す。
    let step = step_new(&mut form, Key::Enter);
    assert!(matches!(step, NewStep::Create(NewRequest::Clone { .. })));
}

#[test]
fn new_project_notice_collapses_git_stderr_to_one_safe_line() {
    // 空メッセージは汎用の一行へフォールバックする。
    assert_eq!(
        new_project_notice(&io::Error::other(String::new())),
        "could not create the project"
    );
    // 複数行の stderr は先頭行だけを trim して残す。
    let multi = io::Error::other("fatal: repository not found\nhint: check the URL");
    assert_eq!(new_project_notice(&multi), "fatal: repository not found");
    // 長い行は省略記号付きで切り詰める。
    let long = io::Error::other("x".repeat(200));
    let notice = new_project_notice(&long);
    assert_eq!(notice.chars().count(), 72);
    assert!(notice.ends_with('…'));
}

#[test]
fn safe_session_error_collapses_daemon_output_to_one_safe_line() {
    // 空メッセージは汎用の一行へフォールバックする。
    assert_eq!(safe_session_error(""), "could not create the session");
    assert_eq!(
        safe_session_error("   \n  "),
        "could not create the session"
    );
    // 複数行の出力は先頭行だけを trim して残す（後続の内部詳細を漏らさない）。
    let multi = "session name already exists\n  at daemon::lifecycle::create (secret path)";
    assert_eq!(safe_session_error(multi), "session name already exists");
    // 長い先頭行は切り詰めず全文を保つ（dialog が幅に合わせて折り返して全文表示する）。
    let notice = safe_session_error(&"x".repeat(200));
    assert_eq!(notice.chars().count(), 200);
    assert!(!notice.contains('…'));
}

#[test]
fn step_new_inserts_navigation_letters_instead_of_treating_them_as_movement() {
    let mut form = New::default();
    step_new(&mut form, Key::Down); // Url
    step_new(&mut form, Key::Char('j'));
    step_new(&mut form, Key::Char('k'));
    assert_eq!(form.focus(), Field::Url);
    assert_eq!(form.url(), "jk");
}

#[test]
fn quitting_from_new_exits_the_runtime() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('e'), Key::Quit]);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[1].join("\n").contains("New Project"));
}

#[test]
fn new_form_enter_creates_a_workspace_and_opens_it() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'), // Welcome → New
        Key::Right,     // Clone → Existing
        Key::Down,      // focus the directory path
        Key::Char('x'), // path "x"; the name derives "x"
        Key::Enter,     // valid → create and open the workspace
        Key::CtrlQ,     // leave the workspace…
        Key::Char('y'), // …confirm
    ]);
    let mut loader = FakeLoader::default();
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // Enter dispatched exactly one create carrying the validated request.
    assert_eq!(
        loader.created,
        vec![NewRequest::Existing {
            path: PathBuf::from("x"),
            name: "x".to_owned(),
        }]
    );
    // The freshly created workspace opened on the same terminal.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("x-session"))
    );
}

#[test]
fn hung_new_create_keeps_ticks_resize_escape_and_quit_responsive() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Other,
        Key::Resize,
        Key::Enter,
        Key::Escape,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        hold_create: true,
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // The second Enter is coalesced while the sole operation is pending.
    assert_eq!(loader.created.len(), 1);
    // Wake-up and resize each advance the spinner and redraw the New frame.
    let loading_frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .filter(|frame| frame.contains("creating workspace"))
        .collect::<Vec<_>>();
    assert!(loading_frames.len() >= 3, "{loading_frames:?}");
    assert_ne!(loading_frames[0], loading_frames[1]);
    // Escape left the hung operation behind and Welcome processed Quit.
    assert!(term.frames.last().unwrap().join("\n").contains("Menu"));
}

#[test]
fn loading_new_can_quit_without_waiting_for_create_completion() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        hold_create: true,
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.created.len(), 1);
}

#[test]
fn cancelled_create_completion_after_reentry_never_opens_the_workspace() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Escape,
        Key::Char('e'),
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        hold_create: true,
        release_after_polls: Some(2),
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.created.len(), 1);
    assert!(
        term.frames
            .iter()
            .all(|frame| !frame.join("\n").contains("Overview"))
    );
    let last_new = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("New Project"))
        .expect("re-entered New frame");
    assert!(last_new.join("\n").contains('x'));
}

#[test]
fn reentered_new_refuses_resubmit_until_cancelled_failure_completes() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Escape,
        Key::Char('e'),
        Key::Enter,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        fail: true,
        hold_create: true,
        release_after_polls: Some(3),
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.created.len(), 1);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("previous creation is still finishing"))
    );
    assert!(frames.iter().any(|frame| frame.contains("open failed")));
}

#[test]
fn stale_and_duplicate_create_completions_open_success_exactly_once() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut loader = FakeLoader {
        completion_noise: true,
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            Vec::new(),
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.created.len(), 1);
    assert_eq!(factory.drops_at_create.len(), 1);
}

/// A workspace the daemon does not serve must not be shown: its session list
/// would be the daemon's workspace under the opened workspace's name (#549).
/// Both switcher entries stay up with the refusal instead of tearing the TUI
/// down, so the workspace that *is* served can be chosen next.
#[test]
fn a_refused_workspace_keeps_the_switcher_open_with_the_reason() {
    const REFUSAL: &str = "cannot open /tmp/recent: this daemon does not serve the selected workspace; \
             this daemon serves the workspace /tmp/served. \
             Stop it with `usagi daemon stop`, then start usagi in /tmp/recent.";

    // Welcome's Recent entry: the refusal shows on Welcome, and no workspace
    // screen is drawn for the workspace that was refused.
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        ..FakeLoader::default()
    };
    assert_eq!(
        run(
            &mut term,
            Vec::new(),
            vec![recent("recent")],
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/recent")]);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    let welcome = frames
        .iter()
        .rev()
        .find(|frame| frame.contains("Menu"))
        .expect("the switcher stays on screen");
    // Wrapped over several lines, so the reason and the recovery step are both
    // present rather than clipped at the terminal width.
    assert!(contains_wrapped(welcome, REFUSAL), "{welcome}");
    assert!(!frames.iter().any(|frame| frame.contains("Overview")));

    // The Open list: same contract, presented on the list itself.
    let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Escape, Key::Quit]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        ..FakeLoader::default()
    };
    assert_eq!(
        run(
            &mut term,
            vec![ws("served")],
            Vec::new(),
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/served")]);
    let open_list = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .rev()
        .find(|frame| frame.contains("Open Workspace"))
        .expect("the Open list stays on screen");
    assert!(contains_wrapped(&open_list, REFUSAL), "{open_list}");
}

/// Whether `frame` shows `text`, ignoring styling and the line breaks the
/// notice was wrapped at.
fn contains_wrapped(frame: &str, text: &str) -> bool {
    let squeeze = |value: &str| {
        crate::presentation::widgets::strip_ansi(value)
            .split_whitespace()
            .collect::<String>()
    };
    squeeze(frame).contains(&squeeze(text))
}

#[test]
fn only_a_refusal_keeps_the_switcher_open() {
    // Every other failure still propagates: staying on the list would not
    // help, and the caller reports it.
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    let error = run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut loader,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "open failed");

    let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Quit]);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    let error = run(
        &mut term,
        vec![ws("served")],
        Vec::new(),
        now(),
        &mut loader,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "open failed");
}

#[test]
fn new_form_enter_keeps_the_draft_and_shows_a_notice_when_creation_fails() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter, // create fails
        Key::Quit,  // then quit from the still-open New form
    ]);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // The create was attempted once and the runtime stayed on the New form.
    assert_eq!(loader.created.len(), 1);
    let last_new = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("New Project"))
        .expect("still on the New screen after a failed create");
    let text = last_new.join("\n");
    assert!(text.contains("open failed")); // the failure notice
    assert!(text.contains('x')); // the draft path is retained
}

#[test]
fn new_form_keeps_the_draft_when_worker_dispatch_fails() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        dispatch_error: Some("worker dispatch failed"),
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert!(loader.created.is_empty());
    let last_new = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("New Project"))
        .expect("still on the New screen after dispatch failed");
    let text = last_new.join("\n");
    assert!(text.contains("worker dispatch failed"));
    assert!(text.contains('x'));
}

#[test]
fn failed_clone_retains_every_clone_draft_field_and_mode() {
    let mut keys = vec![Key::Char('e'), Key::Down];
    keys.extend("https://example.com/acme/app.git".chars().map(Key::Char));
    keys.push(Key::Down);
    keys.extend("/tmp".chars().map(Key::Char));
    keys.push(Key::Down); // derived directory `app`
    keys.push(Key::Down);
    keys.extend("feature".chars().map(Key::Char));
    keys.extend([Key::Enter, Key::Quit]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };

    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(
        loader.created,
        [NewRequest::Clone {
            repository: "https://example.com/acme/app.git".to_owned(),
            destination: PathBuf::from("/tmp/app"),
            branch: Some("feature".to_owned()),
        }]
    );
    let failed = term
        .frames
        .iter()
        .rev()
        .find(|frame| frame.join("\n").contains("open failed"))
        .expect("failed Clone frame");
    let failed = crate::presentation::widgets::strip_ansi(&failed.join("\n"));
    for value in [
        "Clone",
        "https://example.com/acme/app.git",
        "/tmp",
        "app",
        "feature",
    ] {
        assert!(failed.contains(value), "missing {value}: {failed}");
    }
}

#[test]
fn new_form_recovers_after_an_existing_workspace_rejection_and_retries() {
    // The first create is rejected as if the workspace already existed; the
    // user edits the path and the second create succeeds and opens.
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('e'), // Welcome → New
        Key::Right,     // Clone → Existing
        Key::Down,      // focus the directory path
        Key::Char('x'), // path "x"
        Key::Enter,     // create #1 → rejected (already registered)
        Key::Char('y'), // fix the path → "xy" (draft was retained)
        Key::Enter,     // create #2 → succeeds and opens
        Key::CtrlQ,     // leave the workspace…
        Key::Char('y'), // …confirm
    ]);
    let mut loader = FakeLoader {
        create_failures: 1,
        ..FakeLoader::default()
    };
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    // Two attempts: the rejected "x" and the corrected "xy".
    assert_eq!(
        loader.created,
        vec![
            NewRequest::Existing {
                path: PathBuf::from("x"),
                name: "x".to_owned(),
            },
            NewRequest::Existing {
                path: PathBuf::from("xy"),
                name: "xy".to_owned(),
            },
        ]
    );
    // The rejection surfaced a safe notice on the retained New form…
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("already a registered workspace"))
    );
    // …and the corrected retry opened the freshly created workspace.
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("xy-session"))
    );
}

#[test]
fn new_form_enter_clones_and_opens_the_workspace() {
    let mut keys = vec![Key::Char('e'), Key::Down]; // New → focus Url
    keys.extend("https://example.com/o/repo.git".chars().map(Key::Char));
    keys.push(Key::Down); // focus Location
    keys.extend("/tmp".chars().map(Key::Char));
    // Directory は URL から "repo" が導出済み。
    keys.extend([Key::Enter, Key::CtrlQ, Key::Char('y')]);
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    assert_eq!(
        run(&mut term, Vec::new(), Vec::new(), now(), &mut loader).unwrap(),
        Exit::Quit
    );
    assert_eq!(
        loader.created,
        vec![NewRequest::Clone {
            repository: "https://example.com/o/repo.git".to_owned(),
            destination: PathBuf::from("/tmp").join("repo"),
            branch: None,
        }]
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("repo-session"))
    );
}

#[test]
fn open_selection_loads_and_runs_workspace_on_the_same_terminal() {
    let mut term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')]);
    let mut loader = FakeLoader::default();
    assert_eq!(
        run(&mut term, vec![ws("alpha")], Vec::new(), now(), &mut loader,).unwrap(),
        Exit::Quit
    );
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(term.frames.len(), 4);
    assert!(term.frames[0].join("\n").contains("Menu"));
    assert!(term.frames[1].join("\n").contains("Open Workspace"));
    assert!(term.frames[2].join("\n").contains("alpha-session"));
}

#[test]
fn missing_open_selection_confirms_registry_only_removal() {
    let alpha = ws("alpha");
    let mut term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('y'), Key::Quit]);
    let mut loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };

    assert_eq!(
        run(
            &mut term,
            vec![alpha.clone()],
            Vec::new(),
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );

    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames.iter().any(|frame| {
            frame.contains("Workspace not found")
                && frame.contains("/tmp/alpha")
                && frame.contains("remove")
                && frame.contains("cancel")
                && frame.contains("No workspace data is deleted")
        }),
        "missing-workspace prompt was not rendered: {frames:#?}"
    );
    assert_eq!(loader.opened, Vec::<PathBuf>::new());
    assert_eq!(loader.cleanup_calls, 1);
    assert_eq!(loader.cleanup_candidates, vec![vec![alpha.path]]);
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("No workspaces yet"))
    );
}

#[test]
fn missing_workspace_prompt_summarizes_multiple_paths() {
    let prompt = MissingWorkspacePrompt::new(vec!["/tmp/alpha".into(), "/tmp/beta".into()]);
    let frame = render_missing_workspace_prompt(24, 80, &vec![String::new(); 24], &prompt);
    let rendered = strip_ansi(&frame.join("\n"));

    assert!(rendered.contains("2 workspace directories no longer exist"));
    assert!(rendered.contains("Remove their registry entries?"));
}

#[test]
fn missing_workspace_prompt_keyboard_controls_are_complete() {
    let alpha = ws("alpha");
    let mut cancel_term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Right,
        Key::Enter,
        Key::Quit,
    ]);
    let mut cancel_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut cancel_term,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut cancel_loader,
    )
    .unwrap();
    assert_eq!(cancel_loader.cleanup_calls, 0);

    let mut enter_term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Enter, Key::Quit]);
    let mut enter_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut enter_term,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut enter_loader,
    )
    .unwrap();
    assert_eq!(enter_loader.cleanup_calls, 1);

    let mut quit_term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('x'), Key::Quit]);
    let mut quit_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    assert_eq!(
        run(
            &mut quit_term,
            vec![alpha],
            Vec::new(),
            now(),
            &mut quit_loader,
        )
        .unwrap(),
        Exit::Quit
    );
    assert_eq!(quit_loader.cleanup_calls, 0);
}

#[test]
fn missing_recent_can_cancel_without_mutating_the_registry() {
    let alpha = ws("alpha");
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('n'), Key::Quit]);
    let mut loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };

    assert_eq!(
        run(
            &mut term,
            vec![alpha],
            vec![recent("alpha")],
            now(),
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.cleanup_calls, 0);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("Workspace not found"))
    );
    assert!(term.frames.last().unwrap().join("\n").contains("alpha"));

    let alpha = ws("alpha");
    let mut confirm_term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('y'), Key::Quit]);
    let mut confirm_loader = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut confirm_term,
        vec![alpha],
        vec![recent("alpha")],
        now(),
        &mut confirm_loader,
    )
    .unwrap();
    let final_frame = confirm_term.frames.last().unwrap().join("\n");
    assert!(final_frame.contains("No recent workspace"));
    assert!(!final_frame.contains("alpha"));
}

#[test]
fn missing_unite_member_is_preflighted_before_any_workspace_opens() {
    let alpha = ws("alpha");
    let beta = ws("beta");
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Tab,
        Key::Char(' '),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Char('y'),
        Key::Quit,
    ]);
    let mut loader = FakeLoader {
        missing: vec![alpha.path.clone(), beta.path.clone()],
        cleanup_removed: vec![alpha.path.clone(), beta.path.clone()],
        ..FakeLoader::default()
    };

    run(
        &mut term,
        vec![alpha.clone(), beta.clone()],
        Vec::new(),
        now(),
        &mut loader,
    )
    .unwrap();

    assert_eq!(loader.opened, Vec::<PathBuf>::new());
    assert_eq!(
        loader.missing_calls[0],
        vec![alpha.path.clone(), beta.path.clone()]
    );
    assert_eq!(loader.cleanup_candidates, vec![vec![alpha.path, beta.path]]);
    assert!(term.frames.iter().any(|frame| {
        frame
            .join("\n")
            .contains("2 workspace registrations removed")
    }));
}

#[test]
fn unreadable_recent_reports_the_error_without_a_removal_prompt() {
    let alpha = ws("alpha");
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Quit]);
    let mut loader = FakeLoader {
        missing_error: Some(io::ErrorKind::PermissionDenied),
        ..FakeLoader::default()
    };

    run(
        &mut term,
        vec![alpha],
        vec![recent("alpha")],
        now(),
        &mut loader,
    )
    .unwrap();

    assert_eq!(loader.cleanup_calls, 0);
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("workspace path could not be checked"))
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame.contains("Workspace not found"))
    );
}

#[test]
fn restored_or_unreadable_workspace_is_not_unregistered() {
    let alpha = ws("alpha");
    let mut restored_term =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('y'), Key::Quit]);
    let mut restored = FakeLoader {
        missing: vec![alpha.path.clone()],
        cleanup_removed: Vec::new(),
        ..FakeLoader::default()
    };
    run(
        &mut restored_term,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut restored,
    )
    .unwrap();
    assert_eq!(restored.cleanup_calls, 1);
    assert!(restored_term.frames.iter().any(|frame| {
        frame
            .join("\n")
            .contains("Workspace changed while confirming")
    }));

    let mut unreadable_term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Quit]);
    let mut unreadable = FakeLoader {
        missing_error: Some(io::ErrorKind::PermissionDenied),
        ..FakeLoader::default()
    };
    run(
        &mut unreadable_term,
        vec![alpha],
        Vec::new(),
        now(),
        &mut unreadable,
    )
    .unwrap();
    assert_eq!(unreadable.cleanup_calls, 0);
    let frames = unreadable_term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("workspace path could not be checked"))
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame.contains("Workspace not found"))
    );
}

#[test]
fn open_filter_cleanup_confirmation_and_unite_selection_use_the_injected_loader() {
    let alpha = ws("alpha");
    let beta = ws("beta");

    let mut filter = FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('b'), Key::Quit]);
    run(
        &mut filter,
        vec![alpha.clone(), beta.clone()],
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(filter.frames[2].join("\n").contains("↳ /tmp/beta"));

    let mut cancel =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('C'), Key::Char('n'), Key::Quit]);
    let mut cancel_loader = FakeLoader::default();
    run(
        &mut cancel,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut cancel_loader,
    )
    .unwrap();
    assert_eq!(cancel_loader.cleanup_calls, 0);

    let mut confirm =
        FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('C'), Key::Char('y'), Key::Quit]);
    let mut confirm_loader = FakeLoader {
        cleanup_removed: vec![alpha.path.clone()],
        ..FakeLoader::default()
    };
    run(
        &mut confirm,
        vec![alpha.clone()],
        Vec::new(),
        now(),
        &mut confirm_loader,
    )
    .unwrap();
    assert_eq!(confirm_loader.cleanup_calls, 1);
    assert!(confirm.frames[3].join("\n").contains("No workspaces yet"));

    let mut unite = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Tab,
        Key::Char(' '),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    let mut unite_loader = FakeLoader::default();
    run(
        &mut unite,
        vec![alpha, beta],
        Vec::new(),
        now(),
        &mut unite_loader,
    )
    .unwrap();
    assert_eq!(
        unite_loader.opened,
        vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")]
    );
}

#[test]
fn open_unregister_requires_confirmation_and_only_passes_the_selected_path_to_loader() {
    let alpha = ws("alpha");
    let beta = ws("beta");

    let mut cancel = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Down,
        Key::CtrlX,
        Key::Char('c'),
        Key::Quit,
    ]);
    let mut cancel_loader = FakeLoader::default();
    run(
        &mut cancel,
        vec![alpha.clone(), beta.clone()],
        Vec::new(),
        now(),
        &mut cancel_loader,
    )
    .unwrap();
    assert_eq!(cancel_loader.unregister_calls, 0);
    assert!(cancel.frames[3].join("\n").contains("Unregister workspace"));
    assert!(
        cancel.frames[3]
            .join("\n")
            .contains("Only the registry entry is removed. Files stay.")
    );
    assert!(cancel.frames[3].join("\n").contains("beta"));

    let mut confirm = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Down,
        Key::CtrlX,
        Key::Enter,
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    confirm.size = Some((24, 80));
    let mut confirm_loader = FakeLoader::default();
    run(
        &mut confirm,
        vec![alpha, beta.clone()],
        Vec::new(),
        now(),
        &mut confirm_loader,
    )
    .unwrap();
    assert_eq!(confirm_loader.unregister_calls, 1);
    assert_eq!(confirm_loader.unregistered, vec![beta.path]);
    assert!(
        confirm.frames[3]
            .join("\n")
            .contains("Unregister workspace")
    );
    assert!(confirm.frames[4].join("\n").contains("alpha"));
    assert!(!confirm.frames[4].join("\n").contains("beta"));
    let add = confirm
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .find(|frame| frame.contains("Add workspace"))
        .expect("the project add overlay opens");
    assert!(!add.contains("beta"));
}

#[test]
fn open_navigation_keeps_workspace_open_when_escape_is_pressed() {
    // Navigate the Open list to beta and open it, confirm Escape keeps the
    // workspace open, then detach through the controller quit chord.
    let keys = [
        Key::Char('o'),
        Key::Down,
        Key::Up,
        Key::Down,
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    run(
        &mut term,
        vec![ws("alpha"), ws("beta")],
        Vec::new(),
        now(),
        &mut loader,
    )
    .unwrap();
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/beta")]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("beta-session"))
    );
    assert!(term.frames.iter().any(|frame| {
        frame
            .join("\n")
            .contains("a: agent / t: terminal / Enter: actions")
    }));
}

#[test]
fn open_prev_wraps_and_escape_returns_to_welcome() {
    let keys = [
        Key::Char('o'),
        Key::Up,
        Key::Escape,
        Key::Char('q'),
        Key::Enter,
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        vec![ws("alpha"), ws("beta")],
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[1].join("\n").contains("alpha"));
    assert!(term.frames[2].join("\n").contains("beta"));
    assert!(term.frames[3].join("\n").contains("Menu"));
}

#[test]
fn open_touch_keeps_workspace_open_when_escape_is_pressed() {
    let alpha = ws_minutes_ago("alpha", 20);
    let beta = ws_minutes_ago("beta", 10);
    let recent = vec![
        Recent::Workspace(WorkspaceOverview::new(beta.clone(), 2, 3, 4)),
        Recent::Workspace(WorkspaceOverview::new(alpha.clone(), 5, 6, 7)),
    ];
    let keys = [
        Key::Char('o'),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        opened_at: Some(now()),
        ..FakeLoader::default()
    };

    run(&mut term, vec![alpha, beta], recent, now(), &mut loader).unwrap();

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("alpha-session"))
    );
}

#[test]
fn empty_open_enter_stays_and_open_quit_exits() {
    let keys = [Key::Char('o'), Key::Enter, Key::Down, Key::Up, Key::Quit];
    let mut term = FakeTerminal::with_keys(&keys);
    run(
        &mut term,
        Vec::new(),
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert!(term.frames[1].join("\n").contains("No workspaces yet"));
    // Welcome and the empty Open list. Enter, Down and Up have nothing to
    // move in an empty list, so they draw nothing (#554).
    assert_eq!(term.frames.len(), 2);

    let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Tab, Key::Enter, Key::Quit]);
    run(
        &mut term,
        vec![ws("alpha")],
        Vec::new(),
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    // Welcome, the Open list, and the Home frame the chosen workspace
    // opens. Tab completes onto the only entry and changes nothing.
    assert_eq!(term.frames.len(), 3);
}

#[test]
fn open_key_classifier_covers_edit_selection_and_confirmation_paths() {
    let mut open = Open::new(vec![ws("alpha"), ws("Alpha"), ws("beta")]);
    for key in [
        Key::Up,
        Key::Down,
        Key::Char('x'),
        Key::Backspace,
        Key::Left,
        Key::Right,
        Key::Home,
        Key::End,
        Key::LineStart,
        Key::LineEnd,
        Key::Delete,
        Key::SelectLeft,
        Key::SelectRight,
        Key::SelectHome,
        Key::SelectEnd,
        Key::Other,
    ] {
        assert!(matches!(step_open(&mut open, key), OpenStep::Stay));
    }
    assert!(matches!(
        step_open(&mut open, Key::Enter),
        OpenStep::Choose(_)
    ));
    assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Back));
    assert!(matches!(step_open(&mut open, Key::CtrlQ), OpenStep::Quit));

    let _ = step_open(&mut open, Key::Tab);
    let _ = step_open(&mut open, Key::Char(' '));
    let _ = step_open(&mut open, Key::Char(' '));
    let _ = step_open(&mut open, Key::Char(' '));
    assert!(matches!(
        step_open(&mut open, Key::Enter),
        OpenStep::Choose(_)
    ));

    let _ = step_open(&mut open, Key::Char('C'));
    assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Stay));
    let _ = step_open(&mut open, Key::Char('C'));
    assert!(matches!(
        step_open(&mut open, Key::Enter),
        OpenStep::ConfirmCleanup
    ));

    let _ = step_open(&mut open, Key::CtrlX);
    let _ = step_open(&mut open, Key::Left);
    assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Stay));
    let _ = step_open(&mut open, Key::CtrlX);
    assert!(matches!(
        step_open(&mut open, Key::Char('y')),
        OpenStep::ConfirmUnregister(_)
    ));

    for key in [Key::Right, Key::Tab, Key::Char('n'), Key::CtrlQ] {
        let mut open = Open::new(vec![ws("fresh")]);
        let _ = step_open(&mut open, Key::CtrlX);
        let result = step_open(&mut open, key.clone());
        assert!(matches!(result, OpenStep::Stay | OpenStep::Quit));
    }
    let mut open = Open::new(vec![ws("fresh")]);
    let _ = step_open(&mut open, Key::Char('C'));
    assert!(matches!(step_open(&mut open, Key::CtrlQ), OpenStep::Quit));
}

#[test]
fn recent_loads_workspace_and_escape_keeps_it_open() {
    let mut term =
        FakeTerminal::with_keys(&[Key::Char('1'), Key::Escape, Key::CtrlQ, Key::Char('y')]);
    let mut loader = FakeLoader::default();
    run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut loader,
    )
    .unwrap();
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/recent")]);
    assert!(term.frames[1].join("\n").contains("recent-session"));
    assert!(term.frames[2].join("\n").contains("recent-session"));
}

#[test]
fn recent_touch_keeps_workspace_open_when_escape_is_pressed() {
    let alpha = ws_minutes_ago("alpha", 20);
    let beta = ws_minutes_ago("beta", 10);
    let recent = vec![
        Recent::Workspace(WorkspaceOverview::new(beta.clone(), 2, 3, 4)),
        Recent::Workspace(WorkspaceOverview::new(alpha.clone(), 5, 6, 7)),
    ];
    let keys = [Key::Char('2'), Key::Escape, Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        opened_at: Some(now()),
        ..FakeLoader::default()
    };

    run(&mut term, vec![beta, alpha], recent, now(), &mut loader).unwrap();

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert!(term.frames[2].join("\n").contains("alpha-session"));
}

#[test]
fn unite_recent_reopens_the_ordered_workspace_deck() {
    let unite = Recent::Unite(UniteOverview::new(vec![
        WorkspaceOverview::new(ws("primary"), 0, 0, 0),
        WorkspaceOverview::new(ws("other"), 0, 0, 0),
    ]));
    let empty = Recent::Unite(UniteOverview::new(Vec::new()));
    let keys = [Key::Char('2'), Key::Char('1'), Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    run(
        &mut term,
        Vec::new(),
        vec![unite, empty],
        now(),
        &mut loader,
    )
    .unwrap();
    assert_eq!(
        loader.opened,
        vec![PathBuf::from("/tmp/primary"), PathBuf::from("/tmp/other")]
    );
    assert!(term.frames.iter().any(|frame| {
        let text = frame.join("\n");
        text.contains("primary") && text.contains("other")
    }));
}

#[test]
fn missing_recent_number_stays_on_welcome() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('3'), Key::Char('q'), Key::Enter]);
    run(
        &mut term,
        Vec::new(),
        vec![recent("only")],
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    // The out-of-range number leaves Welcome untouched, so it never redraws.
    assert_eq!(term.frames.len(), 1);
}

#[test]
fn quitting_from_a_recent_workspace_exits_the_runtime() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::CtrlQ, Key::Char('y')]);
    run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();
    assert_eq!(term.frames.len(), 3);
    assert!(term.frames[1].join("\n").contains("recent-session"));
}

#[test]
#[allow(clippy::too_many_lines)] // One exhaustive surface-to-help table is easiest to audit intact.
fn workspace_help_resolver_covers_every_frontmost_surface() {
    use super::{
        WorkspaceBaseHelp, WorkspaceDeckHelp, WorkspaceHelpState, resolve_workspace_help_context,
    };
    use crate::presentation::views::key_help::Context as HelpContext;

    let base = WorkspaceHelpState {
        deck: WorkspaceDeckHelp::None,
        overlay: None,
        decision_answer_open: false,
        work_run_mode: super::WorkRunControlMode::Closed,
        director_new_open: false,
        director_route: DirectorRoute::Organization,
        drawer_focus: None,
        base: WorkspaceBaseHelp::Switch,
    };
    assert_eq!(
        WorkspaceDeckHelp::new(false, false),
        WorkspaceDeckHelp::None
    );
    assert_eq!(
        WorkspaceDeckHelp::new(true, true),
        WorkspaceDeckHelp::AddWorkspace
    );
    assert_eq!(
        WorkspaceDeckHelp::new(false, true),
        WorkspaceDeckHelp::WorkspaceFinder
    );
    assert_eq!(
        WorkspaceBaseHelp::new(Route::Home(HomeMode::Switch), false),
        WorkspaceBaseHelp::Switch
    );
    assert_eq!(
        WorkspaceBaseHelp::new(Route::Home(HomeMode::Closeup), false),
        WorkspaceBaseHelp::Closeup
    );
    assert_eq!(
        WorkspaceBaseHelp::new(Route::Home(HomeMode::Closeup), true),
        WorkspaceBaseHelp::LiveTerminal
    );
    assert_eq!(resolve_workspace_help_context(base), HelpContext::Switch);
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            deck: WorkspaceDeckHelp::AddWorkspace,
            ..base
        }),
        HelpContext::AddWorkspace
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            deck: WorkspaceDeckHelp::WorkspaceFinder,
            ..base
        }),
        HelpContext::WorkspaceFinder
    );

    for (overlay, expected) in [
        (Overlay::Overview, HelpContext::Overview),
        (Overlay::Daemon, HelpContext::Daemon),
        (Overlay::Closeup, HelpContext::CloseupActions),
        (Overlay::QuitConfirmation, HelpContext::ExitConfirmation),
        (Overlay::ForceRemoveConfirmation, HelpContext::ForceRemove),
        (Overlay::Notes, HelpContext::Scratchpad),
        (
            Overlay::Environment,
            HelpContext::WorkspaceEnvironmentEditor,
        ),
        (Overlay::Roles, HelpContext::RolesEditor),
        (Overlay::CreateSession, HelpContext::CreateSession),
        (Overlay::Decisions, HelpContext::DecisionList),
        (Overlay::CleanupQueue, HelpContext::CleanupQueue),
        (Overlay::RemoveSessions, HelpContext::RemoveSessions),
        (Overlay::Prs, HelpContext::PullRequests),
        (Overlay::Preview, HelpContext::Preview),
        (Overlay::CreateSessionError, HelpContext::CreateSessionError),
        (
            Overlay::TerminalLaunchError,
            HelpContext::TerminalLaunchError,
        ),
        (Overlay::AgentLaunchError, HelpContext::AgentLaunchError),
        (Overlay::Garden, HelpContext::Garden),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                overlay: Some(overlay),
                ..base
            }),
            expected,
            "{overlay:?}"
        );
    }
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            overlay: Some(Overlay::Decisions),
            decision_answer_open: true,
            ..base
        }),
        HelpContext::DecisionAnswer
    );

    for (mode, expected) in [
        (super::WorkRunControlMode::List, HelpContext::WorkRuns),
        (
            super::WorkRunControlMode::ResolveEscalation,
            HelpContext::WorkRunEscalation,
        ),
        (
            super::WorkRunControlMode::ConfirmCancel,
            HelpContext::WorkRunConfirmation,
        ),
        (
            super::WorkRunControlMode::Submitting,
            HelpContext::WorkRunSubmitting,
        ),
        (
            super::WorkRunControlMode::Retry,
            HelpContext::WorkRunConfirmation,
        ),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                work_run_mode: mode,
                director_route: DirectorRoute::WorkRuns,
                drawer_focus: Some(WorkspaceDrawerFocus::Director),
                ..base
            }),
            expected,
            "{mode:?}"
        );
    }
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: super::WorkRunControlMode::List,
            director_route: DirectorRoute::RunOverview(SupervisorRunId::new()),
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::RunOverview
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: super::WorkRunControlMode::ConfirmDelete,
            director_route: DirectorRoute::WorkRuns,
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::WorkRunConfirmation
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: super::WorkRunControlMode::Submitting,
            director_route: DirectorRoute::Organization,
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::WorkRunSubmitting,
        "an in-flight action outranks the normalized Organization route"
    );

    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            director_new_open: true,
            drawer_focus: Some(WorkspaceDrawerFocus::Director),
            ..base
        }),
        HelpContext::DirectorNew
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            work_run_mode: super::WorkRunControlMode::Submitting,
            director_new_open: true,
            director_route: DirectorRoute::WorkRuns,
            drawer_focus: Some(WorkspaceDrawerFocus::Terminal),
            ..base
        }),
        HelpContext::RootShell,
        "the focused Shell outranks a background Director operation"
    );
    for (drawer_focus, expected) in [
        (WorkspaceDrawerFocus::Director, HelpContext::Organization),
        (WorkspaceDrawerFocus::Terminal, HelpContext::RootShell),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                drawer_focus: Some(drawer_focus),
                ..base
            }),
            expected
        );
    }
    for (director_route, expected) in [
        (
            DirectorRoute::Console(DirectorConsoleParent::Organization),
            HelpContext::DirectorConsole,
        ),
        (
            DirectorRoute::Console(DirectorConsoleParent::RunOverview(SupervisorRunId::new())),
            HelpContext::WorkRunConsole,
        ),
        (
            DirectorRoute::RunOverview(SupervisorRunId::new()),
            HelpContext::RunOverview,
        ),
        (DirectorRoute::WorkRuns, HelpContext::WorkRuns),
    ] {
        assert_eq!(
            resolve_workspace_help_context(WorkspaceHelpState {
                director_route,
                drawer_focus: Some(WorkspaceDrawerFocus::Director),
                ..base
            }),
            expected
        );
    }
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            base: WorkspaceBaseHelp::Closeup,
            ..base
        }),
        HelpContext::Closeup
    );
    assert_eq!(
        resolve_workspace_help_context(WorkspaceHelpState {
            base: WorkspaceBaseHelp::LiveTerminal,
            ..base
        }),
        HelpContext::LiveTerminal
    );
}

#[test]
fn workspace_help_describes_switch_and_swallows_background_commands() {
    use super::{KeyHelpContext, closes_workspace_help};

    assert!(closes_workspace_help(
        &Key::Char('?'),
        KeyHelpContext::Closeup
    ));
    assert!(!closes_workspace_help(
        &Key::Char('?'),
        KeyHelpContext::LiveTerminal
    ));

    let mut term = FakeTerminal::with_keys(&[
        Key::Char('1'),
        Key::Char('?'),
        // Ctrl-X would remove the selected session outside Help.
        Key::CtrlX,
        Key::Char('?'),
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    run(
        &mut term,
        Vec::new(),
        vec![recent("recent")],
        now(),
        &mut FakeLoader::default(),
    )
    .unwrap();

    let help = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .find(|frame| frame.contains("Keyboard help · Workspace switch"))
        .expect("workspace Help frame");
    assert!(help.contains("Ctrl-X"));
    assert!(help.contains("remove session / purge orphan"));
    assert!(!help.contains("Available"));
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("recent-session")),
        "the command pressed behind Help must not remove the session"
    );
}

#[test]
fn key_help_scroll_keys_drive_the_bounded_viewport() {
    use crate::presentation::views::key_help::{Context, State};

    let initial = State::new(
        Context::Switch,
        usagi_core::domain::settings::WorkMode::GoalDriven,
    );
    let mut state = initial;

    assert!(super::scroll_key_help(&mut state, &Key::Up, 5));
    assert_eq!(state, initial);
    assert!(super::scroll_key_help(&mut state, &Key::Down, 5));
    assert_ne!(state, initial);
    assert!(super::scroll_key_help(&mut state, &Key::Home, 5));
    assert_eq!(state, initial);
    assert!(super::scroll_key_help(&mut state, &Key::PageDown, 20));
    assert_ne!(state, initial);
    assert!(super::scroll_key_help(&mut state, &Key::PageUp, 20));
    assert_eq!(state, initial);
    assert!(super::scroll_key_help(&mut state, &Key::End, 20));
    assert_ne!(state, initial);
    assert!(super::scroll_key_help(&mut state, &Key::LineStart, 20));
    assert_eq!(state, initial);
    assert!(super::scroll_key_help(&mut state, &Key::LineEnd, 20));
    assert_ne!(state, initial);
    assert!(!super::scroll_key_help(&mut state, &Key::Other, 20));
}

#[test]
fn workspace_loader_failure_is_propagated() {
    for (keys, recent) in [
        (vec![Key::Char('o'), Key::Enter], Vec::new()),
        (vec![Key::Char('1')], vec![recent("alpha")]),
    ] {
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            fail: true,
            ..FakeLoader::default()
        };
        let error = run(&mut term, vec![ws("alpha")], recent, now(), &mut loader).unwrap_err();
        assert_eq!(error.to_string(), "open failed");
    }
}

struct DefaultTerminalPort;
impl AgentCommandPort for DefaultTerminalPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("agent launch is unavailable".to_owned())
    }
}

#[test]
fn agent_command_port_terminal_methods_are_safe_by_default() {
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let mut port = DefaultTerminalPort;
    assert!(
        port.launch(
            OperationId::new(),
            WorkspaceId::new(),
            Some(SessionId::new()),
            None
        )
        .is_err()
    );
    assert_eq!(
        port.resize_terminal(&terminal, Geometry { cols: 80, rows: 24 }),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.attach_terminal(&terminal, Geometry { cols: 80, rows: 24 }),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.poll_terminal(&terminal, 0),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.input_terminal(
            &terminal,
            TerminalSubscription { id: 1, epoch: 1 },
            0,
            OperationId::new(),
            b"x",
        ),
        Err(TerminalError::Unavailable)
    );
    assert_eq!(
        port.terminal_input_outcome(&terminal, OperationId::new(), 1),
        Err(TerminalError::Unavailable)
    );
    // Detach is a no-op default and must not panic.
    port.detach_terminal(&terminal, TerminalSubscription { id: 1, epoch: 1 });
    assert_eq!(
        port.launch_terminal(
            WorkspaceId::new(),
            Some(SessionId::new()),
            Geometry { cols: 80, rows: 24 },
            "open",
            OperationId::new(),
        ),
        Err("terminal launch is unavailable".to_owned())
    );
    assert_eq!(port.list_terminals(), Err(TerminalError::Unavailable));
}

#[test]
fn key_to_terminal_bytes_encodes_input_and_forwards_control_chords() {
    assert_eq!(key_to_terminal_bytes(Key::Char('a')), Some(b"a".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Enter), Some(b"\r".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::Backspace),
        Some(b"\x7f".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Tab), Some(b"\t".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Escape), Some(b"\x1b".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Up), Some(b"\x1b[A".to_vec()));
    assert_eq!(key_to_terminal_bytes(Key::Down), Some(b"\x1b[B".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::PageUp),
        Some(b"\x1b[5~".to_vec())
    );
    assert_eq!(
        key_to_terminal_bytes(Key::PageDown),
        Some(b"\x1b[6~".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Right), Some(b"\x1b[C".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::SelectRight),
        Some(b"\x1b[C".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Left), Some(b"\x1b[D".to_vec()));
    assert_eq!(
        key_to_terminal_bytes(Key::SelectLeft),
        Some(b"\x1b[D".to_vec())
    );
    for key in [Key::Home, Key::LineStart, Key::SelectHome] {
        assert_eq!(key_to_terminal_bytes(key), Some(vec![1]));
    }
    for key in [Key::End, Key::LineEnd, Key::SelectEnd] {
        assert_eq!(key_to_terminal_bytes(key), Some(vec![5]));
    }
    assert_eq!(
        key_to_terminal_bytes(Key::Delete),
        Some(b"\x1b[3~".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Passthrough(Vec::new())), None);
    assert_eq!(
        key_to_terminal_bytes(Key::Passthrough(vec![0xff])),
        Some(vec![0xff])
    );
    assert_eq!(
        key_to_terminal_bytes(Key::Management {
            action: AppKey::SaveRoles,
            passthrough: vec![0x13],
        }),
        Some(vec![0x13])
    );
    // A program that did not request bracketed paste gets the raw payload;
    // an opted-in Agent gets one marked block. An empty paste sends nothing.
    assert_eq!(
        key_to_terminal_bytes(Key::Paste("a\nb".to_owned())),
        Some(b"a\nb".to_vec())
    );
    assert_eq!(
        key_to_terminal_bytes_for_mode(Key::Paste("a\nb".to_owned()), true),
        Some(b"\x1b[200~a\nb\x1b[201~".to_vec())
    );
    assert_eq!(key_to_terminal_bytes(Key::Paste(String::new())), None);
    assert_eq!(key_to_terminal_bytes(Key::Quit), Some(vec![3]));
    assert_eq!(key_to_terminal_bytes(Key::CtrlQ), Some(vec![17]));
    assert_eq!(key_to_terminal_bytes(Key::CtrlD), Some(vec![4]));
    assert_eq!(key_to_terminal_bytes(Key::CtrlX), Some(vec![24]));
    assert_eq!(key_to_terminal_bytes(Key::Help), None);
    assert_eq!(key_to_terminal_bytes(Key::Other), None);
    assert_eq!(
        key_to_terminal_bytes(Key::Live(
            crate::usecase::terminal_input::LiveTerminalAction::NextTab
        )),
        None
    );
}

#[test]
fn terminal_geometry_uses_the_visible_right_pane_width() {
    assert_eq!(terminal_geometry(24, 80), Geometry { cols: 43, rows: 17 });
    // The left sidebar keeps its 36 columns; every remaining terminal
    // column belongs to the right pane even on a wide outer terminal.
    assert_eq!(
        terminal_geometry(34, 153),
        Geometry {
            cols: 116,
            rows: 27
        }
    );
    assert_eq!(
        foreground_terminal_geometry(
            24,
            100,
            true,
            false,
            false,
            Some(WorkspaceDrawerFocus::Director),
        ),
        Geometry { cols: 56, rows: 16 }
    );
    assert_eq!(
        foreground_terminal_geometry(
            24,
            100,
            false,
            true,
            false,
            Some(WorkspaceDrawerFocus::Terminal),
        ),
        Geometry { cols: 96, rows: 7 }
    );
    assert_eq!(
        foreground_terminal_geometry(
            24,
            100,
            true,
            true,
            false,
            Some(WorkspaceDrawerFocus::Terminal),
        ),
        Geometry { cols: 36, rows: 7 }
    );
    assert_eq!(
        foreground_terminal_geometry(24, 100, false, false, false, None),
        terminal_geometry(24, 100)
    );
}

/// Welcome→Open で開いた workspace が、hard-code の `UnavailableSessionCommandPort`
/// ではなく注入 factory から port を取り出すこと（＝本 fix）を固定する。factory が
/// production では daemon port を返すため、これで全経路が実 port を通ることを担保する。
#[test]
fn open_workspace_pulls_the_session_command_port_from_the_factory() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let created = Arc::new(Mutex::new(0usize));
    let mut factory = SnapshotSessionPortFactory {
        calls: calls.clone(),
        created: created.clone(),
    };
    let keys = [Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;

    assert_eq!(
        run_with_settings(
            &mut term,
            vec![ws("alpha")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(*created.lock().unwrap(), 1);
}

#[test]
fn screen_graph_injects_metrics_when_opening_a_workspace() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut sessions = SnapshotSessionPortFactory {
        calls,
        created: Arc::new(Mutex::new(0)),
    };
    let mut agents = IdleAgentPortFactory;
    let mut metrics = StaticMetricsFactory;
    // Open the workspace, then quit it through the controller's quit chord
    // (Ctrl-Q opens the confirmation, `y` detaches); `q` alone is inert now.
    let keys = [Key::Char('o'), Key::Enter, Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;

    assert_eq!(
        run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
            &mut term,
            vec![ws("alpha")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut sessions,
            &mut agents,
            AvailableAgentModels::all(),
            &mut metrics,
        )
        .unwrap(),
        Exit::Quit
    );

    assert!(
        term.frames
            .iter()
            .flat_map(|frame| frame.iter())
            .any(|line| line.contains('\u{f2db}') && line.contains('\u{f233}'))
    );
}

/// Welcome の Recent 経由で開いた workspace も同じ factory から port を取り出す。
#[test]
fn recent_workspace_pulls_the_session_command_port_from_the_factory() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let created = Arc::new(Mutex::new(0usize));
    let mut factory = SnapshotSessionPortFactory {
        calls: calls.clone(),
        created: created.clone(),
    };
    let keys = [Key::Char('1'), Key::CtrlQ, Key::Char('y')];
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader::default();
    let mut settings = DefaultSettingsPort;

    assert_eq!(
        run_with_settings(
            &mut term,
            Vec::new(),
            vec![recent("home")],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/home")]);
    assert_eq!(*created.lock().unwrap(), 1);
}

#[test]
fn render_home_snapshot_draws_the_initial_home_surface() {
    // The non-interactive `usagi open <path>` fallback renders one static
    // project bar plus Home frame through the controller projection: the
    // workspace name, its sessions, and both creation affordances.
    let rows = render_home_snapshot(30, 100, &snapshot("demo"), IconMode::NerdFont);
    assert_eq!(rows.len(), 30);
    assert!(rows[0].contains("1 demo"));
    assert!(rows[0].contains("+ Open"));
    let frame = rows.join("\n");
    assert!(frame.contains("demo"));
    assert!(frame.contains("demo-session"));
    assert!(frame.contains("+ new session"));
    // A zero size safely falls back to the default geometry.
    assert!(!render_home_snapshot(0, 0, &snapshot("demo"), IconMode::NerdFont).is_empty());

    let text =
        strip_ansi(&render_home_snapshot(30, 100, &snapshot("demo"), IconMode::Text).join("\n"));
    for nerd_font_glyph in ["\u{f0ec}", "\u{f00e}", "\u{f085}"] {
        assert!(!text.contains(nerd_font_glyph));
    }
    assert!(text.contains("switch  closeup"));
    assert!(text.contains("Agents"));

    // A Failed session in the snapshot renders with its failed treatment and
    // failure reason, so the initial fallback frame surfaces it too.
    let mut failed_snapshot = snapshot("demo");
    let id = failed_snapshot.session_ids[0];
    failed_snapshot.session_lifecycles.insert(
        id,
        usagi_core::domain::session_lifecycle::SessionLifecycleProjection {
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Failed,
            failure_stage: Some(usagi_core::domain::session_lifecycle::FailureStage::Create),
            failure_summary: Some("branch exists".into()),
        },
    );
    let failed_frame =
        render_home_snapshot(30, 100, &failed_snapshot, IconMode::NerdFont).join("\n");
    assert!(failed_frame.contains("failed"));
    assert!(failed_frame.contains("branch exists"));
}

#[test]
fn session_command_result_message_carries_no_projection() {
    let result = SessionCommandResult::message("daemon accepted");
    assert_eq!(result.message, "daemon accepted");
    assert!(result.sessions.is_none());
    assert!(result.session_ids.is_none());
}

#[test]
fn public_value_derives_are_exercised() {
    let snapshot = snapshot("derive");
    assert_eq!(snapshot.clone(), snapshot);
    assert!(format!("{snapshot:?}").contains("derive"));
    let quit = Exit::Quit;
    assert_eq!(quit.clone(), Exit::Quit);
    assert!(format!("{quit:?}").contains("Quit"));
    let welcome = Exit::Welcome;
    assert_eq!(welcome.clone(), Exit::Welcome);
    assert_ne!(welcome, quit);
    assert!(format!("{welcome:?}").contains("Welcome"));
}

fn info() -> AppInfo {
    AppInfo {
        name: "usagi",
        version: "0.1.0",
    }
}

#[test]
fn write_banner_writes_description_line() {
    let mut buf = Vec::new();
    write_banner(&mut buf, &info()).unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "usagi v0.1.0\n");
}

#[test]
fn banner_screen_runner_names_non_interactive_tui_screens() {
    let entries = [
        EntryScreen::Welcome,
        EntryScreen::Workspace {
            path: PathBuf::from("/tmp/project"),
        },
        EntryScreen::Config,
    ];
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::new(&mut buf, &info);
    for entry in &entries {
        dispatch(entry, &mut runner).unwrap();
    }
    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "usagi v0.1.0: welcome TUI\n\
             usagi v0.1.0: workspace TUI (/tmp/project)\n\
             usagi v0.1.0: config TUI\n"
    );
}

#[test]
fn doctor_runner_renders_checks_and_summary() {
    use crate::usecase::doctor::{CheckStatus, DiagnosticCheck, DoctorReport};

    let report = DoctorReport {
        checks: vec![
            DiagnosticCheck {
                name: "Git",
                status: CheckStatus::Pass,
                detail: "git version 2.50".to_owned(),
            },
            DiagnosticCheck {
                name: "Codex CLI",
                status: CheckStatus::Warning,
                detail: "not found".to_owned(),
            },
            DiagnosticCheck {
                name: "Daemon",
                status: CheckStatus::Fail,
                detail: "connection refused".to_owned(),
            },
        ],
    };
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::with_doctor_report(&mut buf, &info, &report);
    dispatch(&EntryScreen::Doctor, &mut runner).unwrap();

    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "usagi v0.1.0: doctor\n\
             [ok] Git: git version 2.50\n\
             [warn] Codex CLI: not found\n\
             [error] Daemon: connection refused\n\
             result: problems found\n"
    );
}

#[test]
fn doctor_runner_requires_a_report() {
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::new(&mut buf, &info);
    assert_eq!(
        dispatch(&EntryScreen::Doctor, &mut runner)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn doctor_runner_renders_a_healthy_summary() {
    use crate::usecase::doctor::DoctorReport;

    let report = DoctorReport { checks: Vec::new() };
    let mut buf = Vec::new();
    let info = info();
    let mut runner = BannerScreenRunner::with_doctor_report(&mut buf, &info, &report);
    dispatch(&EntryScreen::Doctor, &mut runner).unwrap();
    assert!(
        String::from_utf8(buf)
            .unwrap()
            .ends_with("result: healthy\n")
    );
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("write failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn banner_screen_runner_propagates_write_failure() {
    let mut out = FailingWriter;
    out.flush().unwrap();
    let info = info();
    let mut runner = BannerScreenRunner::new(&mut out, &info);
    assert_eq!(
        dispatch(&EntryScreen::Welcome, &mut runner)
            .unwrap_err()
            .to_string(),
        "write failed"
    );
}

// ---- #510: interrupted Agent tabs and their explicit per-tab resume ----

use super::{ExactAgentResume, InterruptedTab};
use usagi_core::domain::agent::{AgentResumeRelation, AgentResumeTarget};

/// A daemon port whose exact-target resume answers with a scripted result and
/// counts how many requests it received.
struct ScriptedExactResumePort {
    answers: Vec<Result<ExactAgentResume, String>>,
    requests: Arc<Mutex<Vec<(AgentResumeTarget, OperationId)>>>,
}

impl AgentCommandPort for ScriptedExactResumePort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("agent launch is unavailable".to_owned())
    }

    fn resume_exact(
        &mut self,
        target: AgentResumeTarget,
        operation_id: OperationId,
    ) -> Result<ExactAgentResume, String> {
        self.requests.lock().unwrap().push((target, operation_id));
        if self.answers.is_empty() {
            return Err("no scripted answer".to_owned());
        }
        self.answers.remove(0)
    }
}

/// Wait until the scripted port has received `expected` exact-resume
/// requests. The worker runs off-thread, so the count is polled rather than
/// sampled after a fixed sleep.
fn await_requests(requests: &Arc<Mutex<Vec<(AgentResumeTarget, OperationId)>>>, expected: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let observed = requests.lock().unwrap().len();
        assert!(observed <= expected, "more resume requests than expected");
        if observed == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the resume worker did not reach {expected} requests"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// One interrupted lineage of `session`, resumable unless `resumable` is false.
fn interrupted_history(
    workspace: WorkspaceId,
    session: Option<SessionId>,
    resumable: bool,
) -> InterruptedTab {
    use usagi_core::domain::agent::{ProviderKind, ProviderResumePhase, ProviderResumeReason};
    use usagi_core::domain::id::{AgentResumeSourceId, AgentRuntimeId, WorktreeId};

    let continuation = AgentContinuationRef::new();
    let worktree_id = WorktreeId::new();
    InterruptedTab {
        continuation,
        session_id: session,
        last_terminal: TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: session,
            worktree_id,
        },
        provider: Some(ProviderKind::Claude),
        last_known_phase: Some(ProviderResumePhase::Interrupted),
        reason: if resumable {
            ProviderResumeReason::ExplicitResumeAvailable
        } else {
            ProviderResumeReason::ProviderMetadataUnavailable
        },
        target: resumable.then(|| AgentResumeTarget {
            continuation,
            source: AgentResumeSourceId::new(),
            workspace_id: workspace,
            session_id: session,
            worktree_id,
            runtime_id: AgentRuntimeId::new(),
            adapter_revision: 3,
        }),
    }
}

/// The accepted answer one resume of `history` would produce.
fn exact_resume_answer(history: &InterruptedTab) -> ExactAgentResume {
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: history.last_terminal.workspace_id,
        session_id: history.session_id,
        worktree_id: history.last_terminal.worktree_id,
    };
    ExactAgentResume {
        terminal: terminal.clone(),
        continuation: Some(history.continuation),
        relation: Some(AgentResumeRelation {
            source: history.target.as_ref().unwrap().source,
            replacement_runtime: usagi_core::domain::id::AgentRuntimeId::new(),
            replacement_terminal: terminal,
        }),
    }
}

/// A shell driven into Closeup on `session` whose pane restores `history`.
fn closeup_with_history(
    workspace: WorkspaceId,
    session: SessionId,
    history: Vec<InterruptedTab>,
    launch: Box<dyn PaneLaunchCommandPort>,
) -> (WorkspaceIoRuntime, WorkspaceRuntime) {
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_pane_launch_port(launch)
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(MemoryIntentPort {
                state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                mutations: Arc::new(Mutex::new(Vec::new())),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Enter);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        interaction,
        revision,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: None,
            interrupted: history,
        }],
    ));
    (ui, runtime)
}

#[test]
fn interrupted_history_joins_its_own_scope_in_the_restore_projection() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let other = SessionId::new();
    let root_history = interrupted_history(workspace, None, true);
    let session_history = interrupted_history(workspace, Some(session), true);
    let second_session_history = interrupted_history(workspace, Some(session), false);

    let targets = super::pane_restore_targets(
        workspace,
        &BTreeSet::from([session, other]),
        AgentTabProjection::default(),
        &[],
        None,
        vec![
            root_history.clone(),
            session_history.clone(),
            second_session_history.clone(),
        ],
        &BTreeMap::from([(Some(session), second_session_history.continuation)]),
    );

    let root = targets
        .iter()
        .find(|target| target.target == Target::Root(workspace))
        .unwrap();
    assert_eq!(
        root.interrupted
            .iter()
            .map(|tab| tab.continuation)
            .collect::<Vec<_>>(),
        vec![root_history.continuation]
    );
    let managed = targets
        .iter()
        .find(|target| target.target == Target::Session(session))
        .unwrap();
    // Several histories in one scope stay separate tabs, in projection order.
    assert_eq!(
        managed.selected_interrupted,
        Some(second_session_history.continuation)
    );
    assert_eq!(
        managed
            .interrupted
            .iter()
            .map(|tab| tab.continuation)
            .collect::<Vec<_>>(),
        vec![
            session_history.continuation,
            second_session_history.continuation
        ]
    );
    // A session without history keeps an empty entry rather than borrowing
    // another scope's tabs.
    let empty = targets
        .iter()
        .find(|target| target.target == Target::Session(other))
        .unwrap();
    assert!(empty.interrupted.is_empty());
}

#[test]
fn one_explicit_resume_sends_one_request_and_turns_only_that_tab_live() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resumed = interrupted_history(workspace, Some(session), true);
    let untouched = interrupted_history(workspace, Some(session), true);
    let answer = exact_resume_answer(&resumed);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![resumed.clone(), untouched.clone()],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![Ok(answer.clone())],
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    // Nothing has asked the daemon to resume anything yet.
    assert!(requests.lock().unwrap().is_empty());
    super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert_eq!(ui.pane_launches.len(), 1);
    // A repeated activation converges to the in-flight request.
    super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert_eq!(ui.pane_launches.len(), 1);

    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    std::thread::sleep(std::time::Duration::from_millis(20));
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    // Exactly one daemon request, carrying the daemon's own opaque target.
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, *resumed.target.as_ref().unwrap());
    assert_eq!(runtime.focused_terminal(), Some(answer.terminal));
    // The other history tab is unchanged and still unresumed.
    assert_eq!(runtime.active_pane().tabs().len(), 2);
    assert_eq!(
        runtime
            .active_pane()
            .tabs()
            .iter()
            .filter(|tab| matches!(tab, PaneTab::Interrupted(_)))
            .count(),
        1
    );
}

#[test]
fn selecting_a_resumable_interrupted_tab_starts_its_exact_resume() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests,
        })),
    );

    let mut pending_targets = std::collections::HashMap::new();
    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);

    assert_eq!(ui.pane_launches.len(), 1);
    assert!(matches!(
        ui.pane_launches.first(),
        Some(PaneLaunch::ResumeExact { .. })
    ));
}

#[test]
fn managed_host_tab_cycle_selects_pending_and_activates_interrupted_history() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let continuation = history.continuation;
    let last_terminal = history.last_terminal.clone();
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        Box::new(UnavailablePaneLaunchPort),
    );
    ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
        session_id: Some(session),
        continuation,
        terminal: last_terminal,
        select: true,
    })
    .unwrap();
    let operation = OperationId::new();
    runtime.on_effect(&Effect::LaunchAgent {
        workspace,
        session: Some(session),
        operation_id: operation,
        profile: None,
    });
    let mut pending_targets = std::collections::HashMap::new();

    let (sender, receiver) = std::sync::mpsc::channel();
    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Next))
        .unwrap();
    drain_host_actions(&receiver, &mut ui, &mut runtime, &mut pending_targets);
    assert_eq!(
        runtime.active_pane().selected(),
        &PaneSelection::Tab(TabSelection::Pending(operation))
    );

    sender
        .send(ControllerHostAction::SelectTab(TabDirection::Previous))
        .unwrap();
    drain_host_actions(&receiver, &mut ui, &mut runtime, &mut pending_targets);
    assert_eq!(
        runtime.focused_interrupted().map(|tab| tab.continuation),
        Some(continuation)
    );
    assert_eq!(
        runtime
            .interrupted_removal_confirmation()
            .map(|prompt| prompt.tab().continuation),
        Some(continuation)
    );
}

#[test]
fn selecting_an_unresumable_tab_prompts_then_keeps_or_removes_exact_history() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let continuation = history.continuation;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending_targets = std::collections::HashMap::new();

    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    let prompt = runtime
        .interrupted_removal_confirmation()
        .expect("an unresumable selection opens its removal prompt");
    assert_eq!(prompt.tab().continuation, continuation);
    assert!(!prompt.is_confirm_selected());
    let frame = render_home_material(&home_frame_material(
        20,
        80,
        &runtime,
        "demo",
        &[],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    ))
    .join("\n");
    assert!(frame.contains("Remove interrupted Agent"));
    assert!(frame.contains("Enter: select"));
    assert!(frame.contains("y: remove"));
    assert!(frame.contains("Esc/n: keep"));
    assert!(frame.contains("[ remove"));
    assert!(frame.contains("[ keep   ]"));
    assert!(frame.contains("kept no resume metadata"));

    // The prompt owns all input. A terminal EOF cannot leak to a pane
    // behind it, and choosing Keep leaves both durable and local history.
    assert!(handle_interrupted_removal_confirmation(
        &Key::CtrlD,
        &mut ui,
        &mut runtime,
    ));
    assert!(runtime.interrupted_removal_confirmation().is_some());
    assert!(handle_interrupted_removal_confirmation(
        &Key::Enter,
        &mut ui,
        &mut runtime,
    ));
    assert!(runtime.interrupted_removal_confirmation().is_none());
    assert!(runtime.active_pane().has_tabs());
    assert!(ui.agent_dismissed().is_empty());

    // Escape is the explicit Keep shortcut and also leaves history intact.
    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    assert!(handle_interrupted_removal_confirmation(
        &Key::Escape,
        &mut ui,
        &mut runtime,
    ));
    assert!(runtime.interrupted_removal_confirmation().is_none());
    assert!(runtime.active_pane().has_tabs());

    // Selecting the same tab again reopens a fresh safe-default prompt.
    assert!(select_right_pane_tab(&mut ui, &mut runtime, 0));
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    assert!(handle_interrupted_removal_confirmation(
        &Key::Left,
        &mut ui,
        &mut runtime,
    ));
    assert!(handle_interrupted_removal_confirmation(
        &Key::Enter,
        &mut ui,
        &mut runtime,
    ));
    assert!(!runtime.active_pane().has_tabs());
    assert_eq!(ui.agent_dismissed(), BTreeSet::from([continuation]));
    assert!(requests.lock().unwrap().is_empty());
    assert!(ui.pane_launches.is_empty());
    super::confirm_interrupted_removal(&mut ui, &mut runtime);
}

#[test]
fn unresumable_prompt_y_shortcut_removes_the_exact_history() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    // `y` remains a direct, explicit Remove shortcut independent of focus.
    let y_history = interrupted_history(workspace, Some(session), false);
    let y_continuation = y_history.continuation;
    let (mut y_ui, mut y_runtime) = closeup_with_history(
        workspace,
        session,
        vec![y_history],
        Box::new(UnavailablePaneLaunchPort),
    );
    assert!(select_right_pane_tab(&mut y_ui, &mut y_runtime, 0));
    activate_focused_interrupted_tab(
        &mut y_ui,
        &mut y_runtime,
        &mut std::collections::HashMap::new(),
    );
    assert!(handle_interrupted_removal_confirmation(
        &Key::Char('y'),
        &mut y_ui,
        &mut y_runtime,
    ));
    assert!(!y_runtime.active_pane().has_tabs());
    assert_eq!(y_ui.agent_dismissed(), BTreeSet::from([y_continuation]));
}

#[test]
fn selecting_an_interrupted_rabbit_opens_the_same_unresumable_prompt() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let runtime_id = AgentRuntimeId::new();
    let continuation = history.continuation;
    let terminal = history.last_terminal.clone();
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        Box::new(UnavailablePaneLaunchPort),
    );
    ui.agent_inventory = Some(AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(runtime_id, terminal, Some(session)).unwrap(),
            continuation,
            state: AgentRuntimeInventoryState::Interrupted,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    });
    let mut pending_targets = std::collections::HashMap::new();
    let mut pointer_gesture = false;
    let _ = runtime.apply_event(AppEvent::IdleElapsed(GARDEN_IDLE_THRESHOLD));
    let record = SessionRecord {
        name: "interrupted".to_owned(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: PathBuf::from("/tmp/demo/interrupted"),
        created_at: now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: Vec::new(),
    };
    let material = home_frame_material(
        24,
        80,
        &runtime,
        "demo",
        &[ProjectedSession::from_record(session, &record)],
        None,
        health(),
        &BTreeMap::new(),
        None,
        None,
        now(),
    )
    .with_agent_inventory(ui.agent_inventory(), runtime.panes());
    let click = (0..24)
        .flat_map(|row| (0..80).map(move |column| (column, row)))
        .find(|&(column, row)| {
            matches!(
                garden_click_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    column,
                    row,
                ),
                Some(GardenClick::Visit {
                    agent: Some(selected),
                    ..
                }) if selected == runtime_id
            )
        })
        .expect("the interrupted rabbit is clickable");

    assert_eq!(
        route_garden_input(
            &mut ui,
            &mut runtime,
            Some(&material),
            &Key::Click {
                column: click.0,
                row: click.1,
            },
            &mut pointer_gesture,
        ),
        Some(GardenInputRoute::Agent(Vec::new())),
    );
    activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
    assert_eq!(
        runtime
            .interrupted_removal_confirmation()
            .map(|prompt| prompt.tab().continuation),
        Some(continuation),
    );
    assert!(ui.pane_launches.is_empty());
}

#[test]
fn a_refused_or_failed_resume_keeps_the_history_tab_with_safe_feedback() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let mut relationless = exact_resume_answer(&history);
    relationless.relation = None;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history.clone()],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![
                Err("provider resume failed; refresh Agent inventory".to_owned()),
                Ok(relationless),
            ],
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    for _ in 0..2 {
        super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );
        // The tab survives every refusal, and no live pane is invented.
        assert_eq!(runtime.active_pane().tabs().len(), 1);
        assert!(matches!(
            runtime.active_pane().tabs()[0],
            PaneTab::Interrupted(_)
        ));
        assert!(runtime.focused_terminal().is_none());
        assert!(runtime.active_pane().error().is_some());
    }
    // A transport failure and a relation-less answer are both retryable, so
    // both requests reached the daemon.
    assert_eq!(requests.lock().unwrap().len(), 2);
}

#[test]
fn an_unresumable_history_tab_never_reaches_the_daemon() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), false);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert!(ui.pane_launches.is_empty());
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(
        runtime.active_pane().error(),
        Some(
            crate::usecase::application::interrupted_tab::ResumeRejection::NotResumable
                .safe_message()
        )
    );
}

#[test]
fn closing_an_inventory_only_history_tab_persists_its_removal_without_resuming() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history.clone()],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    assert!(ui.agent_slot_order().is_empty());
    assert_eq!(
        runtime.focused_interrupted().map(|tab| tab.continuation),
        Some(history.continuation),
        "an interrupted-only pane must be selectable without opening another Agent"
    );

    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::CloseTab),
        &mut ui,
        &mut runtime,
        &mut controls,
        &mut term,
        &mut browser,
        &mut pending,
        20,
        80,
        0,
        0,
    ));

    assert!(!runtime.active_pane().has_tabs());
    assert_eq!(ui.agent_slot_order(), vec![history.continuation]);
    assert_eq!(ui.agent_dismissed(), BTreeSet::from([history.continuation]));
    assert!(runtime.active_pane().error().is_none());
    assert!(runtime.state().notice().is_none());
    assert!(requests.lock().unwrap().is_empty());
    assert!(ui.pane_launches.is_empty());

    // A later coherent inventory replay receives the durable dismissal and
    // therefore cannot resurrect the tab that Ctrl-O x removed.
    let inventory = AgentInventory {
        workspace_id: workspace,
        runtimes: vec![AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(
                history.target.as_ref().unwrap().runtime_id,
                history.last_terminal.clone(),
                Some(session),
            )
            .unwrap(),
            continuation: history.continuation,
            state: AgentRuntimeInventoryState::Interrupted,
            resumed_from: None,
        }],
        resumable: Vec::new(),
    };
    assert!(
        crate::usecase::application::interrupted_tab::project(
            &inventory,
            workspace,
            &BTreeSet::from([session]),
            &ui.agent_slot_order(),
            &ui.agent_dismissed(),
            &BTreeSet::new(),
        )
        .tabs
        .is_empty()
    );
}

#[test]
fn the_resume_chord_drives_the_selected_history_tab_through_the_live_surface() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let answer = exact_resume_answer(&history);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history.clone()],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![Ok(answer.clone())],
            requests: Arc::clone(&requests),
        })),
    );
    let mut controls = LiveTerminalControls::default();
    let mut term = FakeTerminal::default();
    let mut browser = UnavailableBrowserOpener;
    let mut pending_targets = std::collections::HashMap::new();

    // `Ctrl-O r` is a pane-only control: it is consumed by the Closeup pane.
    assert!(intercept_live_terminal_control(
        &Key::Live(LiveTerminalAction::ResumeTab),
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
    assert_eq!(ui.pane_launches.len(), 1);

    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    std::thread::sleep(std::time::Duration::from_millis(20));
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending_targets,
        terminal_geometry(20, 80),
    );
    assert_eq!(runtime.focused_terminal(), Some(answer.terminal));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn a_queued_resume_waits_for_the_daemon_port_and_never_duplicates_its_request() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let first = interrupted_history(workspace, Some(session), true);
    let second = interrupted_history(workspace, Some(session), true);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![first.clone(), second.clone()],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![
                Ok(exact_resume_answer(&first)),
                Ok(exact_resume_answer(&second)),
            ],
            requests: Arc::clone(&requests),
        })),
    );
    let mut pending = std::collections::HashMap::new();

    // Resume both history tabs before either answer arrives.
    super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    let _ = runtime.select_tab(TabDirection::Next);
    super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert_eq!(ui.pane_launches.len(), 2);

    // Only one worker may own the stateful daemon port: the second request
    // stays queued instead of starting a second concurrent resume.
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(ui.pane_launches.len(), 1);
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    assert_eq!(ui.pane_launches.len(), 1);
    await_requests(&requests, 1);

    // Once the port returns with the first answer the queued one runs.
    for _ in 0..2 {
        super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
        std::thread::sleep(std::time::Duration::from_millis(20));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            terminal_geometry(20, 80),
        );
    }
    await_requests(&requests, 2);
    assert!(
        runtime
            .active_pane()
            .tabs()
            .iter()
            .all(|tab| matches!(tab, PaneTab::Live(_)))
    );
}

#[test]
fn a_resume_without_an_agent_context_or_a_selected_history_tab_does_nothing() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut pending = std::collections::HashMap::new();

    // No daemon Agent context at all.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut bare = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    super::resume_focused_interrupted_tab(&mut bare, &mut runtime, &mut pending);
    assert!(bare.pane_launches.is_empty());

    // An Agent context with no active managed target stops at the runtime
    // target boundary before looking for an interrupted tab.
    let view = WorkspaceView::with_runtime_ids(ws("demo"), empty_state("demo"), Vec::new());
    let mut inactive = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort));
    let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
    super::resume_focused_interrupted_tab(&mut inactive, &mut runtime, &mut pending);
    assert!(inactive.pane_launches.is_empty());

    // An Agent context whose selected tab is live, not interrupted.
    let history = interrupted_history(workspace, Some(session), true);
    let (mut ui, mut runtime) = closeup_with_history(
        workspace,
        session,
        vec![history],
        launch_port(Box::new(ScriptedExactResumePort {
            answers: Vec::new(),
            requests: Arc::new(Mutex::new(Vec::new())),
        })),
    );
    let live = live_terminal_ref(workspace, session);
    let operation = OperationId::new();
    let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
    let _ = runtime.complete_pane(Target::Session(session), operation, live.clone());
    let _ = runtime.focus_terminal(Target::Session(session), live);
    super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    assert!(ui.pane_launches.is_empty());
}

#[test]
fn an_accepted_resume_whose_display_intent_cannot_be_saved_surfaces_a_typed_notice() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let history = interrupted_history(workspace, Some(session), true);
    let answer = exact_resume_answer(&history);
    let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
    let mut ui = WorkspaceIoRuntime::new(view, Box::new(UnavailableSessionCommandPort))
        .with_agent_context(
            workspace,
            vec![session],
            Box::new(UnavailableAgentCommandPort),
        )
        .with_pane_launch_port(launch_port(Box::new(ScriptedExactResumePort {
            answers: vec![Ok(answer.clone())],
            requests: Arc::new(Mutex::new(Vec::new())),
        })))
        .with_agent_tab_intent(
            workspace,
            BTreeSet::from([session]),
            Box::new(FailingIntentPort {
                state: Arc::new(Mutex::new(AgentTabIntent::empty(workspace))),
                error: AgentTabIntentError::Unavailable,
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
        );
    let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
    let _ = runtime.handle_key(Key::Down);
    let _ = runtime.handle_key(Key::Enter);
    let (interaction, revision) = runtime.restore_fence();
    assert!(runtime.restore_snapshot(
        interaction,
        revision,
        vec![super::PaneRestoreTarget {
            target: Target::Session(session),
            panes: Vec::new(),
            selected: None,
            selected_interrupted: None,
            interrupted: vec![history],
        }],
    ));
    let _ = runtime.select_tab(TabDirection::Next);
    let mut pending = std::collections::HashMap::new();

    super::resume_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending);
    super::drain_pane_launches(&mut ui, terminal_geometry(20, 80));
    std::thread::sleep(std::time::Duration::from_millis(20));
    super::drain_pane_completions_into_runtime(
        &mut ui,
        &mut runtime,
        &mut pending,
        terminal_geometry(20, 80),
    );

    // A daemon success is not shown as committed until display intent is
    // durable. The interrupted tab stays in place and the typed failure is
    // visible, so neither pane state nor intent bytes claims success.
    assert!(runtime.focused_terminal().is_none());
    assert!(runtime.focused_interrupted().is_some());
    assert!(runtime.state().notice().is_some());
}

/// One counted stand-in for every port slot of a workspace composition.
///
/// It answers like the `Unavailable*` ports and counts its own drop, so
/// "nothing this workspace established survives into the next one" is a
/// single number rather than a per-port inspection (#556).
struct CountedPort(Arc<AtomicUsize>);

impl Drop for CountedPort {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl SessionCommandPort for CountedPort {}

impl SessionRefreshPort for CountedPort {
    fn wake(&mut self) {}

    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        None
    }
}

impl AgentCommandPort for CountedPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent launch is unavailable".to_owned())
    }
}

impl PaneLaunchCommandPort for CountedPort {
    fn launch(
        &self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent launch is unavailable".to_owned())
    }

    fn resume(
        &self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent resume is unavailable.".to_owned())
    }

    fn resume_exact(
        &self,
        _target: super::AgentResumeTarget,
        _operation: OperationId,
    ) -> Result<super::ExactAgentResume, String> {
        Err("Exact Agent resume is unavailable.".to_owned())
    }

    fn launch_terminal(
        &self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        _operation: OperationId,
    ) -> Result<TerminalRef, String> {
        Err("terminal launch is unavailable".to_owned())
    }
}

impl super::RestoreConnectionPort for CountedPort {
    fn take_reconnected_epoch(&mut self) -> Option<u64> {
        None
    }
}

impl AgentTabIntentPort for CountedPort {
    fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
        Ok(AgentTabIntent::empty(workspace))
    }

    fn mutate(
        &mut self,
        workspace: WorkspaceId,
        _expected_revision: u64,
        mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
        let mut intent = AgentTabIntent::empty(workspace);
        let projection = intent.apply(mutation);
        Ok(AgentTabIntentPortCommit {
            intent,
            projection,
            mutation_applied: true,
            cas_conflict: false,
        })
    }
}

impl ExternalTerminalPort for CountedPort {
    fn open(&mut self, _directory: &Path) -> Result<(), String> {
        Err("external terminal launch is unavailable".to_owned())
    }
}

impl MetricsPort for CountedPort {
    fn latest(&mut self) -> Option<DaemonMetrics> {
        None
    }
}

impl BrowserOpener for CountedPort {
    fn open(&mut self, _url: &str) -> Result<(), String> {
        Err("browser opening is unavailable".to_owned())
    }
}

impl super::SessionWorktreeScanPort for CountedPort {
    fn scan(&mut self, _workspace: &Path) -> Vec<String> {
        Vec::new()
    }
}

impl super::SessionCatalogPort for CountedPort {
    fn roles(&self, _workspace: &Path) -> super::SessionRoleCatalog {
        super::SessionRoleCatalog::default()
    }

    fn branches(
        &self,
        _workspace: &Path,
        _configured_default: Option<&str>,
    ) -> super::SessionBranchCatalog {
        super::SessionBranchCatalog::default()
    }

    fn branch_worker(&self) -> Box<dyn super::SessionBranchCatalogPort> {
        Box::new(super::UnavailableSessionBranchCatalogPort)
    }
}

impl super::GardenInventoryPort for CountedPort {
    fn inventory(&mut self, _workspace: WorkspaceId) -> Result<AgentWorkspaceObservation, String> {
        Err("Agent inventory is unavailable".to_owned())
    }
}

impl BackendDecisionPort for CountedPort {
    fn refresh(&mut self, _workspace: WorkspaceId, _completions: Completions) {}

    fn resolve(
        &mut self,
        _workspace: WorkspaceId,
        _decision_id: UserDecisionId,
        _answer: UserDecisionAnswer,
        _completions: Completions,
    ) {
    }
}

/// Ports of one composition that the frame loop itself owns for the whole
/// life of the workspace, and therefore drops by returning.
///
/// The restore client is deliberately excluded and left uncounted: it is the
/// one port handed to a detached worker, because quitting must never wait for
/// a hung restore observation (#551, fixed by
/// `blocked_restore_inventory_never_blocks_render_or_quit`). Its drop
/// therefore happens on that worker and is not ordered against the next
/// workspace's composition. Branch discovery also uses a detached worker, but
/// its adapter is freshly created by the counted resident catalog port and
/// shares no teardown-sensitive resource with it.
const RESIDENT_PORTS_PER_COMPOSITION: usize = 13;

/// A production-shaped factory whose every port counts its own drop, and
/// which records how many ports had been dropped when each workspace's
/// composition was created.
struct CountingBackendFactory {
    drops: Arc<AtomicUsize>,
    /// `drops` observed at the start of each `create`, in entry order.
    drops_at_create: Vec<usize>,
}

impl CountingBackendFactory {
    fn new() -> Self {
        Self {
            drops: Arc::new(AtomicUsize::new(0)),
            drops_at_create: Vec::new(),
        }
    }

    fn port(&self) -> CountedPort {
        CountedPort(Arc::clone(&self.drops))
    }
}

#[test]
fn branch_catalog_worker_does_not_keep_the_resident_catalog_alive() {
    let drops = Arc::new(AtomicUsize::new(0));
    let catalog: Box<dyn super::SessionCatalogPort> = Box::new(CountedPort(Arc::clone(&drops)));
    let worker = catalog.branch_worker();

    drop(catalog);

    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        worker.branches(Path::new("/tmp/workspace"), None),
        super::SessionBranchCatalog::default()
    );
}

impl super::ControllerBackendFactory for CountingBackendFactory {
    fn create(
        &mut self,
        _: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> super::ControllerBackendComposition {
        self.drops_at_create.push(self.drops.load(Ordering::SeqCst));
        super::ControllerBackendComposition {
            backend: DaemonBackend::new(
                Box::new(host.clone()),
                Box::new(host),
                Box::new(UnavailableBackendPort),
                Box::new(UnavailableBackendPort),
            )
            .with_decisions(Box::new(self.port()))
            .with_overlay(Box::new(UnavailableBackendPort)),
            // Counted resident port. Its worker factory returns a separate,
            // deliberately stateless adapter that may finish after this frame.
            session_catalogs: Box::new(self.port()),
            session_commands: Box::new(self.port()),
            session_refresh: Box::new(self.port()),
            agent_commands: Box::new(self.port()),
            pane_launch_commands: Box::new(self.port()),
            // Uncounted on purpose: see `RESIDENT_PORTS_PER_COMPOSITION`.
            restore_commands: Box::new(UnavailableAgentCommandPort),
            session_worktrees: Box::new(self.port()),
            restore_connection: Box::new(self.port()),
            // Owned by the loop unless a Garden round is in flight, which
            // needs the screen saver to be up: these entries never open it.
            garden_inventory: Box::new(self.port()),
            work_runs: Box::new(super::UnavailableWorkRunPort),
            agent_tab_intents: Box::new(self.port()),
            external_terminal: Box::new(self.port()),
            metrics: Box::new(self.port()),
            browser: Box::new(self.port()),
        }
    }
}

/// A Recent entry with a pinned `updated_at`, so the switcher's order — and
/// therefore which number key opens which workspace — is deterministic.
fn recent_at(name: &str, updated_at: DateTime<Utc>) -> Recent {
    let mut workspace = ws(name);
    workspace.updated_at = updated_at;
    Recent::Workspace(WorkspaceOverview::new(workspace, 1, 0, 0))
}

/// `usagi open <path>` that could not reach a daemon lands in the switcher
/// instead of back on the shell, so the very first frame has to say why the
/// workspace it asked for is not on screen.
#[test]
fn a_notice_seeded_switcher_opens_with_the_reason_on_its_first_frame() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('q'), Key::Enter]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend_and_notice(
            &mut term,
            Vec::new(),
            vec![recent_at("first", now())],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
            Some("daemon unavailable: the daemon did not answer".to_owned()),
        )
        .unwrap(),
        Exit::Quit
    );

    let first = term.frames.first().expect("the switcher drew no frame");
    assert!(
        first
            .iter()
            .any(|line| line.contains("the daemon did not answer")),
        "the first frame must carry the reason: {first:?}"
    );
    // Nothing was opened: the notice explains an absence, it does not stand
    // in for a workspace that did load.
    assert!(loader.opened.is_empty());
}

/// A daemon that cannot be reached must not end the process from an entry
/// screen. It is the same shape of failure as a workspace this daemon does
/// not serve — the switcher stays up with the reason — and collapsing to the
/// shell here is exactly how a wedged daemon locks a user out of usagi.
#[test]
fn an_unreachable_daemon_keeps_the_switcher_up_instead_of_ending_the_process() {
    let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('q'), Key::Enter]);
    let mut loader = FakeLoader {
        unreachable: Some("daemon unavailable: the daemon did not answer".to_owned()),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            Vec::new(),
            vec![recent_at("first", now())],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/first")]);
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.iter().any(|line| line.contains("did not answer"))),
        "the switcher must show why the workspace did not open"
    );
    // The outage happened before any workspace runtime existed, so no daemon
    // port was created for a workspace that never opened.
    assert_eq!(factory.drops.load(Ordering::SeqCst), 0);
}

/// #556 acceptance. Home can return to Welcome, another workspace opens from
/// there, and none of it needs a restarted process. All three entries —
/// Recent, Open, New — lead back to the switcher, and `Exit::Quit` stays the
/// process-exit answer reached only by choosing `quit`.
#[test]
fn every_workspace_entry_returns_to_welcome_without_restarting() {
    // Recent `1` (first) → leave → Open `o`/↓/Enter (second) → leave →
    // New Existing (`x`) → leave → quit from Welcome. Each `w` is the exit
    // prompt's leave answer.
    let mut keys = vec![Key::Char('1'), Key::CtrlQ, Key::Char('w')];
    keys.extend([
        Key::Char('o'),
        Key::Down,
        Key::Enter,
        Key::CtrlQ,
        Key::Char('w'),
    ]);
    keys.extend([
        Key::Char('e'),
        Key::Right,
        Key::Down,
        Key::Char('x'),
        Key::Enter,
        Key::CtrlQ,
        Key::Char('w'),
    ]);
    // Back on Welcome for the third time, `q` ends the process.
    keys.push(Key::Char('q'));
    let mut term = FakeTerminal::with_keys(&keys);
    let mut loader = FakeLoader {
        opened_at: Some(now() + Duration::hours(1)),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            Vec::new(),
            vec![
                recent_at("first", now()),
                recent_at("second", now() - Duration::hours(1)),
            ],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    // Three distinct workspaces opened by the one process, in entry order,
    // each one rebinding the settings port to its own root. (`loader.opened`
    // records the New form's relative path; the snapshot resolves it.)
    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/first"),
            PathBuf::from("/tmp/second"),
            PathBuf::from("x"),
        ]
    );
    assert_eq!(
        settings.selected,
        vec![
            PathBuf::from("/tmp/first"),
            PathBuf::from("/tmp/second"),
            PathBuf::from("/tmp/x"),
        ]
    );
    assert_eq!(factory.drops_at_create.len(), 3);

    // Every departure landed on Welcome — the `Menu` heading belongs to the
    // switcher alone, so it is absent from the Open list and the New form
    // that were used to get there.
    let frames = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .collect::<Vec<_>>();
    let mut from = 0;
    for workspace in ["first", "second", "x"] {
        let home = frames
            .iter()
            .skip(from)
            .position(|frame| frame.contains(workspace))
            .unwrap_or_else(|| panic!("{workspace} opens"))
            + from;
        let welcome = frames
            .iter()
            .skip(home + 1)
            .position(|frame| frame.contains("Menu"))
            .unwrap_or_else(|| panic!("leaving {workspace} draws Welcome"))
            + home
            + 1;
        assert!(
            !frames[welcome].contains("Open Workspace"),
            "leaving {workspace} must land on Welcome, not the Open list: {}",
            frames[welcome]
        );
        from = welcome;
    }
}

/// A workspace whose settings cannot be bound is a failure to report, not a
/// silent entry: the error propagates out of the screen graph instead of the
/// graph continuing with the previous workspace's settings. Every entry —
/// Recent, Open, New — propagates it the same way.
#[test]
fn a_settings_binding_failure_while_opening_a_workspace_propagates() {
    struct UnbindableSettings;

    impl SettingsPort for UnbindableSettings {
        fn select_workspace(&mut self, _workspace_root: &Path) -> io::Result<()> {
            Err(io::Error::other("settings directory is unavailable"))
        }

        fn read(
            &mut self,
            _scope: usagi_core::usecase::settings::SettingsScope,
        ) -> io::Result<Settings> {
            Ok(Settings::default())
        }

        fn save(
            &mut self,
            _scope: usagi_core::usecase::settings::SettingsScope,
            _settings: &Settings,
        ) -> io::Result<()> {
            Ok(())
        }
    }

    let cases = [
        (
            vec![Key::Char('1')],
            Vec::new(),
            vec![recent_at("first", now())],
        ),
        (
            vec![Key::Char('o'), Key::Enter],
            vec![ws("listed")],
            Vec::new(),
        ),
        (
            vec![
                Key::Char('e'),
                Key::Right,
                Key::Down,
                Key::Char('x'),
                Key::Enter,
            ],
            Vec::new(),
            Vec::new(),
        ),
    ];

    for (keys, workspaces, recent) in cases {
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader::default();
        let mut factory = CountingBackendFactory::new();

        let error = run_screen_graph_with_backend(
            &mut term,
            workspaces,
            recent,
            now(),
            Start::Welcome,
            &mut loader,
            &mut UnbindableSettings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "settings directory is unavailable");
        // The failure happened before any daemon port was created, so nothing
        // was established for a workspace that never opened.
        assert!(factory.drops_at_create.is_empty());
        assert_eq!(factory.drops.load(Ordering::SeqCst), 0);
    }
}

/// #556 acceptance: leaving tears the workspace down. Every resident port of
/// the first composition — including the session catalog — is dropped before
/// the second composition exists. Detached restore and branch adapters have an
/// explicitly separate lifetime and own no resident connection.
#[test]
fn leaving_a_workspace_drops_every_port_before_the_next_one_is_created() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('w'),
        Key::Char('2'),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        opened_at: Some(now() + Duration::hours(1)),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        Vec::new(),
        vec![
            recent_at("first", now()),
            recent_at("second", now() - Duration::hours(1)),
        ],
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    // Exactly two compositions, and the second one started with the first
    // one already fully torn down: residue would show as a shortfall here.
    assert_eq!(
        factory.drops_at_create,
        vec![0, RESIDENT_PORTS_PER_COMPOSITION]
    );
    // After the run, the second composition is gone too: nothing outlives it.
    assert_eq!(
        factory.drops.load(Ordering::SeqCst),
        2 * RESIDENT_PORTS_PER_COMPOSITION
    );
}

#[test]
fn project_deck_add_and_digit_switch_drop_the_old_composition_before_create() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Live(LiveTerminalAction::ActivateWorkspace(1)),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
        ]
    );
    assert_eq!(
        factory.drops_at_create,
        vec![
            0,
            RESIDENT_PORTS_PER_COMPOSITION,
            2 * RESIDENT_PORTS_PER_COMPOSITION,
        ]
    );
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
fn project_add_applies_a_registry_refresh_while_the_overlay_is_open() {
    let alpha = ws("alpha");
    let beta = ws("beta");
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        registry_refresh: Some(FakeRegistryRefresh::Queued(vec![alpha.clone(), beta])),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![alpha],
        Vec::new(),
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    assert_eq!(loader.registry_refresh_dispatches, 1);
    assert_eq!(
        loader.opened,
        vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")]
    );
}

#[test]
fn add_workspace_overlay_can_close_its_checked_active_project() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::CtrlX,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(factory.drops_at_create, vec![0]);
    assert!(term.frames.last().unwrap().join("\n").contains("Recent"));
}

#[test]
fn project_arrows_follow_tab_order_and_ctrl_option_reaches_closeup() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Left,
        Key::Right,
        Key::Right,
        Key::Enter,
        Key::Live(LiveTerminalAction::NextWorkspace),
        Key::Live(LiveTerminalAction::PreviousWorkspace),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            vec![ws("alpha"), ws("beta")],
            Vec::new(),
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
        ]
    );
    assert_eq!(
        factory.drops_at_create,
        vec![
            0,
            RESIDENT_PORTS_PER_COMPOSITION,
            2 * RESIDENT_PORTS_PER_COMPOSITION,
            3 * RESIDENT_PORTS_PER_COMPOSITION,
            4 * RESIDENT_PORTS_PER_COMPOSITION,
            5 * RESIDENT_PORTS_PER_COMPOSITION,
            6 * RESIDENT_PORTS_PER_COMPOSITION,
        ]
    );
}

#[test]
fn project_navigation_chords_reach_closeup_but_yield_to_foreground_surfaces() {
    let alpha = snapshot("alpha");
    let beta = snapshot("beta");
    let gamma = snapshot("gamma");
    let deck = WorkspaceDeck::from_snapshots(&[alpha.clone(), beta, gamma]).unwrap();
    let mut state = AppState::home(alpha.workspace_id, alpha.session_ids.clone());

    assert_eq!(
        super::workspace_navigation_target(&deck, &state, &Key::Left),
        Some(PathBuf::from("/tmp/gamma"))
    );
    assert_eq!(
        super::workspace_navigation_target(&deck, &state, &Key::Right),
        Some(PathBuf::from("/tmp/beta"))
    );
    assert_eq!(
        super::workspace_navigation_target(&deck, &state, &Key::Up),
        None
    );

    let _ =
        crate::usecase::application::controller::update(&mut state, AppEvent::Key(AppKey::Enter));
    assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
    assert_eq!(
        super::workspace_navigation_target(&deck, &state, &Key::Right),
        None
    );
    assert_eq!(
        super::workspace_navigation_target(
            &deck,
            &state,
            &Key::Live(LiveTerminalAction::PreviousWorkspace),
        ),
        Some(PathBuf::from("/tmp/gamma"))
    );
    assert_eq!(
        super::workspace_navigation_target(
            &deck,
            &state,
            &Key::Live(LiveTerminalAction::NextWorkspace),
        ),
        Some(PathBuf::from("/tmp/beta"))
    );

    let mut state = AppState::home(alpha.workspace_id, alpha.session_ids.clone());
    let _ =
        crate::usecase::application::controller::update(&mut state, AppEvent::Key(AppKey::CtrlQ));
    assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
    assert_eq!(
        super::workspace_navigation_target(
            &deck,
            &state,
            &Key::Live(LiveTerminalAction::PreviousWorkspace),
        ),
        None
    );

    let mut overlay_deck = deck;
    overlay_deck.open_switcher();
    let state = AppState::home(alpha.workspace_id, alpha.session_ids);
    assert_eq!(
        super::workspace_navigation_target(
            &overlay_deck,
            &state,
            &Key::Live(LiveTerminalAction::NextWorkspace),
        ),
        None
    );
}

#[test]
fn project_add_preserves_an_open_workspace_draft() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::Agent),
        Key::Paste("keep-me".to_owned()),
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    term.size = Some((24, 80));
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha"), ws("beta")],
        Vec::new(),
        now(),
        Start::Welcome,
        &mut loader,
        &mut settings,
        &mut factory,
        AvailableAgentModels::all(),
    )
    .unwrap();

    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    assert_eq!(factory.drops_at_create, vec![0]);
    assert!(
        term.frames.iter().any(|frame| {
            contains_wrapped(
                &frame.join("\n"),
                "Save or cancel the current draft before switching.",
            )
        }),
        "{:#?}",
        term.frames
    );
    assert!(
        term.frames
            .iter()
            .any(|frame| frame.join("\n").contains("keep-me"))
    );
}

#[test]
fn project_bar_click_adds_and_activates_the_project_identity_it_rendered() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Click { column: 10, row: 0 },
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Click { column: 2, row: 0 },
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    term.size = Some((24, 80));
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha"), ws("beta")],
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
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/alpha"),
        ]
    );
    assert!(term.frames.iter().any(|frame| frame[0].contains("+ Open")));
}

#[test]
fn project_prepare_failure_keeps_the_current_composition_and_deck() {
    const REFUSAL: &str = "another daemon owns beta; retry after it releases the workspace";
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        refuse_paths: vec![PathBuf::from("/tmp/beta")],
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha"), ws("beta")],
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
        vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")]
    );
    assert_eq!(factory.drops_at_create, vec![0]);
}

#[test]
fn project_batch_settings_failure_is_all_or_nothing() {
    let mut term = FakeTerminal::with_keys(&[
        Key::Char('o'),
        Key::Enter,
        Key::Live(LiveTerminalAction::OpenWorkspace),
        Key::Down,
        Key::Char(' '),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader::default();
    let mut settings = WorkspaceBindingSettingsPort {
        refuse: Some(PathBuf::from("/tmp/gamma")),
        ..WorkspaceBindingSettingsPort::default()
    };
    let mut factory = CountingBackendFactory::new();

    run_screen_graph_with_backend(
        &mut term,
        vec![ws("alpha"), ws("beta"), ws("gamma")],
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
        vec![
            PathBuf::from("/tmp/alpha"),
            PathBuf::from("/tmp/beta"),
            PathBuf::from("/tmp/gamma"),
        ]
    );
    assert_eq!(factory.drops_at_create, vec![0]);
    assert!(settings.selected.ends_with(&[
        PathBuf::from("/tmp/beta"),
        PathBuf::from("/tmp/gamma"),
        PathBuf::from("/tmp/alpha"),
    ]));
}

#[test]
fn project_registry_path_fence_covers_empty_match_and_mismatch() {
    let alpha = snapshot("alpha");
    assert!(!registry_contains_path(&[], &alpha.workspace.path));
    assert!(registry_contains_path(
        std::slice::from_ref(&alpha.workspace),
        &alpha.workspace.path,
    ));
    assert!(!registry_contains_path(
        std::slice::from_ref(&alpha.workspace),
        Path::new("/tmp/beta"),
    ));

    let beta = snapshot("beta");
    let mut registry = vec![alpha.workspace, beta.workspace.clone()];
    remove_registry_paths(&mut registry, &[]);
    remove_registry_paths(&mut registry, &[PathBuf::from("/tmp/missing")]);
    remove_registry_paths(&mut registry, &[PathBuf::from("/tmp/alpha")]);
    assert_eq!(registry, vec![beta.workspace]);
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers the deck helpers' shared fallback state.
fn project_deck_composition_helpers_cover_safe_fallbacks() {
    let alpha = snapshot("alpha");
    assert!(!workspace_has_unsaved_surface(&WorkspaceRuntime::new(
        alpha.workspace_id,
        Vec::new(),
    )));
    let mut deck = WorkspaceDeck::new(&alpha);
    let mut term = FakeTerminal::default();

    let mut no_loader: Option<&mut dyn WorkspaceLoader> = None;
    restore_prepared_workspace(&mut no_loader, Path::new("/tmp/alpha"));
    assert!(
        prepare_deck_workspace(
            &mut term,
            &mut no_loader,
            &mut deck,
            Path::new("/tmp/beta"),
            "Opening…",
        )
        .is_none()
    );
    assert!(deck.notice().unwrap().contains("workspace list"));

    let mut successful_loader = FakeLoader::default();
    let mut loader: Option<&mut dyn WorkspaceLoader> = Some(&mut successful_loader);
    assert_eq!(
        prepare_deck_workspace(
            &mut term,
            &mut loader,
            &mut deck,
            Path::new("/tmp/beta"),
            "Opening…",
        )
        .unwrap()
        .workspace
        .path,
        PathBuf::from("/tmp/beta")
    );

    let mut failed_loader = FakeLoader {
        fail: true,
        ..FakeLoader::default()
    };
    let mut loader: Option<&mut dyn WorkspaceLoader> = Some(&mut failed_loader);
    assert!(
        prepare_deck_workspace(
            &mut term,
            &mut loader,
            &mut deck,
            Path::new("/tmp/beta"),
            "Opening…",
        )
        .is_none()
    );
    assert_eq!(deck.notice(), Some("open failed"));

    let mut no_config = None;
    assert!(prepare_activation_settings(
        &mut no_config,
        &mut no_loader,
        &mut deck,
        Path::new("/tmp/alpha"),
        Path::new("/tmp/beta"),
    ));
    assert!(prepare_batch_settings(
        &mut no_config,
        &mut no_loader,
        &mut deck,
        Path::new("/tmp/alpha"),
        &[],
    ));

    let mut settings = WorkspaceBindingSettingsPort {
        refuse: Some(PathBuf::from("/tmp/beta")),
        ..WorkspaceBindingSettingsPort::default()
    };
    let mut rollback_loader = FakeLoader::default();
    let mut rollback: Option<&mut dyn WorkspaceLoader> = Some(&mut rollback_loader);
    let mut context = Some(WorkspaceConfigContext {
        settings: &mut settings,
        available_models: AvailableAgentModels::all(),
    });
    assert!(!prepare_activation_settings(
        &mut context,
        &mut rollback,
        &mut deck,
        Path::new("/tmp/alpha"),
        Path::new("/tmp/beta"),
    ));
    assert_eq!(deck.notice(), Some("workspace settings are unreadable"));

    assert_eq!(
        adjust_project_bar_pointer(Key::Click { column: 4, row: 2 }),
        Key::Click { column: 4, row: 1 }
    );
    assert_eq!(
        adjust_project_bar_pointer(Key::Click { column: 4, row: 0 }),
        Key::Click { column: 4, row: 0 }
    );
    assert_eq!(
        adjust_project_bar_pointer(Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 4,
            row: 2,
        })),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column: 4,
            row: 1,
        })
    );
    assert_eq!(adjust_project_bar_pointer(Key::Other), Key::Other);

    assert_eq!(
        prepare_workspace_deck(&mut term, &mut FakeLoader::default(), &[])
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        recent_paths(&Recent::Workspace(WorkspaceOverview::new(
            alpha.workspace,
            0,
            0,
            0,
        ))),
        vec![PathBuf::from("/tmp/alpha")]
    );
    assert!(recent_paths(&Recent::Unite(UniteOverview::new(Vec::new()))).is_empty());
}

#[test]
fn deck_preparation_retries_an_empty_lifecycle_snapshot_for_garden() {
    let mut fake = FakeLoader {
        open_snapshot: FakeOpenSnapshot::Empty,
        ..FakeLoader::default()
    };
    let paths = vec![PathBuf::from("/tmp/alpha"), PathBuf::from("/tmp/beta")];
    let mut term = FakeTerminal::default();
    let (_, active, mut deck) = prepare_workspace_deck(&mut term, &mut fake, &paths).unwrap();

    assert_eq!(fake.opened, paths);
    assert_eq!(fake.refreshed, paths);
    assert_eq!(active.session_ids.len(), 1);
    assert_eq!(deck.garden_projection(&[]).1.len(), 1);

    fake.opened.clear();
    fake.refreshed.clear();
    let mut loader: Option<&mut dyn WorkspaceLoader> = Some(&mut fake);
    let gamma = prepare_deck_workspace(
        &mut term,
        &mut loader,
        &mut deck,
        Path::new("/tmp/gamma"),
        "Opening…",
    )
    .expect("an added project is prepared");
    assert_eq!(gamma.session_ids.len(), 1);
    assert_eq!(fake.opened, vec![PathBuf::from("/tmp/gamma")]);
    assert_eq!(fake.refreshed, vec![PathBuf::from("/tmp/gamma")]);
}

/// #556 acceptance: the workspace fence still refuses, and it refuses as a
/// notice on the screen the user is standing on — including the Welcome that
/// was reached by leaving a workspace. No silent fallback to the workspace
/// that was left.
#[test]
fn a_fenced_workspace_refuses_on_the_welcome_reached_by_leaving() {
    const REFUSAL: &str = "cannot open /tmp/second: this daemon does not serve the selected \
             workspace; this daemon serves the workspace /tmp/first.";

    let mut term = FakeTerminal::with_keys(&[
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('w'),
        // The fenced workspace keeps Welcome up; the served one still opens.
        Key::Char('2'),
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut loader = FakeLoader {
        refuse: Some(REFUSAL.to_owned()),
        refuse_paths: vec![PathBuf::from("/tmp/second")],
        opened_at: Some(now() + Duration::hours(1)),
        ..FakeLoader::default()
    };
    let mut settings = WorkspaceBindingSettingsPort::default();
    let mut factory = CountingBackendFactory::new();

    assert_eq!(
        run_screen_graph_with_backend(
            &mut term,
            Vec::new(),
            vec![
                recent_at("first", now()),
                recent_at("second", now() - Duration::hours(1)),
            ],
            now(),
            Start::Welcome,
            &mut loader,
            &mut settings,
            &mut factory,
            AvailableAgentModels::all(),
        )
        .unwrap(),
        Exit::Quit
    );

    // The refused open was attempted and reported; only the served workspace
    // ever became a composition.
    assert_eq!(
        loader.opened,
        vec![
            PathBuf::from("/tmp/first"),
            PathBuf::from("/tmp/second"),
            PathBuf::from("/tmp/first"),
        ]
    );
    assert_eq!(factory.drops_at_create.len(), 2);
    let welcome = term
        .frames
        .iter()
        .map(|frame| frame.join("\n"))
        .rev()
        .find(|frame| frame.contains("Menu"))
        .expect("the switcher stays on screen after the refusal");
    assert!(contains_wrapped(&welcome, REFUSAL), "{welcome}");
}

/// A workspace opened directly (`usagi open <path>`) has no Welcome behind it, so
/// the runner reports the choice and the composition root decides. Quitting
/// and leaving must be different answers here too.
#[test]
fn a_direct_workspace_reports_leaving_and_quitting_as_different_exits() {
    for (key, expected) in [
        (Key::Char('w'), Exit::Welcome),
        (Key::Char('q'), Exit::Quit),
    ] {
        let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, key.clone()]);
        let mut factory = FixedBackendFactory {
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
            run_workspace_controller_with_backend(&mut term, snapshot("direct"), &mut factory,)
                .unwrap(),
            expected,
            "{key:?}"
        );
    }
}

fn has_director_drawer(frames: &[Vec<String>]) -> bool {
    frames.iter().any(|frame| {
        let text = frame.join("\n");
        text.contains("♛ Director")
            && (text.contains("No conversations yet") || text.contains("Organization"))
            && text.contains("[ New ]")
    })
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
        Key::Char('1'),
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

/// A terminal that only implements the required port methods keeps the old
/// pacing: `wait_for_key` waits the frame out and reports no input.
#[derive(Default)]
struct SleepingTerminal {
    frames: usize,
    waits: Vec<std::time::Duration>,
}

impl Terminal for SleepingTerminal {
    fn size(&mut self) -> io::Result<(usize, usize)> {
        Ok((24, 80))
    }

    fn draw(&mut self, _frame: &[String]) -> io::Result<()> {
        self.frames += 1;
        Ok(())
    }

    fn wait(&mut self, duration: std::time::Duration) -> io::Result<()> {
        self.waits.push(duration);
        Ok(())
    }

    fn read_key(&mut self) -> io::Result<Key> {
        Ok(Key::Quit)
    }
}

/// Feeds one key at a chosen splash frame; every other frame reports the
/// keys given for it, then no input.
struct SplashTerminal {
    frames: Vec<Vec<String>>,
    answers: VecDeque<Option<Key>>,
    keys: VecDeque<Key>,
}

impl SplashTerminal {
    fn new(answers: Vec<Option<Key>>) -> Self {
        Self {
            frames: Vec::new(),
            answers: answers.into(),
            keys: VecDeque::new(),
        }
    }

    fn with_keys(mut self, keys: &[Key]) -> Self {
        self.keys = keys.iter().cloned().collect();
        self
    }
}

impl Terminal for SplashTerminal {
    fn size(&mut self) -> io::Result<(usize, usize)> {
        Ok((24, 80))
    }

    fn draw(&mut self, frame: &[String]) -> io::Result<()> {
        self.frames.push(frame.to_vec());
        Ok(())
    }

    fn wait(&mut self, _duration: std::time::Duration) -> io::Result<()> {
        panic!("the splash waits on the input-aware path, not on a bare sleep");
    }

    fn read_key(&mut self) -> io::Result<Key> {
        self.keys
            .pop_front()
            .ok_or_else(|| io::Error::other("no more keys"))
    }

    fn wait_for_key(&mut self, _duration: std::time::Duration) -> io::Result<Option<Key>> {
        Ok(self.answers.pop_front().flatten())
    }
}

/// #556 acceptance: the splash is skippable. A key press during it ends the
/// animation at that frame instead of holding the terminal for its full
/// 14-frame run.
#[test]
fn a_key_press_skips_the_rest_of_the_startup_splash() {
    let frames = crate::presentation::views::splash::FRAMES;
    // No input at all: every frame plays, as before.
    let mut full = SplashTerminal::new(vec![None; frames]);
    assert_eq!(play_startup_splash(&mut full).unwrap(), frames);
    assert_eq!(full.frames.len(), frames);

    // A key on the third frame ends it there.
    let mut skipped = SplashTerminal::new(vec![None, None, Some(Key::Char('o'))]);
    assert_eq!(play_startup_splash(&mut skipped).unwrap(), 3);
    assert_eq!(skipped.frames.len(), 3);

    // A wake-up tick and a resize are not key presses: they keep the pace,
    // and the next frame re-reads the terminal size.
    let mut paced = SplashTerminal::new(
        std::iter::repeat_n(Some(Key::Other), frames / 2)
            .chain(std::iter::repeat_n(Some(Key::Resize), frames - frames / 2))
            .collect(),
    );
    assert_eq!(play_startup_splash(&mut paced).unwrap(), frames);
    assert_eq!(paced.frames.len(), frames);
}

#[test]
fn a_terminal_without_an_input_aware_wait_keeps_the_old_splash_pacing() {
    let mut term = SleepingTerminal::default();

    let played = play_startup_splash(&mut term).unwrap();

    let frames = crate::presentation::views::splash::FRAMES;
    assert_eq!(played, frames);
    assert_eq!(term.frames, frames);
    assert_eq!(term.waits.len(), frames);
    assert!(
        term.waits
            .iter()
            .all(|wait| *wait == crate::presentation::views::splash::ANIM_TICK)
    );
}

/// #556 acceptance: the splash belongs to launching the process, not to
/// arriving at Welcome. The Welcome reached by leaving a workspace draws no
/// splash frame at all.
#[test]
fn the_startup_splash_plays_once_per_process() {
    let mut splash = super::StartupSplash::new();
    let frames = crate::presentation::views::splash::FRAMES;

    let mut term = SplashTerminal::new(vec![None; frames]);
    assert_eq!(splash.play(&mut term).unwrap(), frames);
    assert_eq!(term.frames.len(), frames);

    // Returning to Welcome is not a launch: no size read, no draw, no wait.
    let mut again = SplashTerminal::new(Vec::new());
    assert_eq!(splash.play(&mut again).unwrap(), 0);
    assert!(again.frames.is_empty());
}

/// #556 acceptance: an interrupted splash still hands Welcome its correct
/// initial state. The skip key is consumed by the skip, so Welcome starts on
/// its own first frame and reads its own first input.
#[test]
fn welcome_starts_correctly_after_an_interrupted_splash() {
    let mut term = SplashTerminal::new(vec![Some(Key::Char('o'))]).with_keys(&[
        Key::Char('1'),
        Key::CtrlQ,
        Key::Char('q'),
    ]);
    let mut splash = super::StartupSplash::new();
    let played = splash.play(&mut term).unwrap();
    assert_eq!(played, 1);

    let mut loader = FakeLoader::default();
    assert_eq!(
        run_from_start(
            &mut term,
            Vec::new(),
            vec![recent_at("first", now())],
            now(),
            Start::Welcome,
            &mut loader,
        )
        .unwrap(),
        Exit::Quit
    );

    // The frame right after the skipped splash is the switcher, drawn from
    // the given Recent; Welcome's own keys then drive it as usual.
    let welcome = term.frames[played].join("\n");
    assert!(welcome.contains("Menu"), "{welcome}");
    assert!(welcome.contains("first"), "{welcome}");
    assert_eq!(loader.opened, vec![PathBuf::from("/tmp/first")]);
}

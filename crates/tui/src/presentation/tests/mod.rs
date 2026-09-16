#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

mod director;
mod flow;
mod garden;
mod home;
mod render;
mod restore;
mod session;
mod terminal;
mod work_run;
mod workspace_rows;
use super::{
    AgentCommandPort, AgentCommandPortFactory, AgentPaneAdmission, AgentTabIntentPort,
    AgentTabIntentPortCommit, BTreeMap, BannerScreenRunner, BrowserOpener, Config, ConfigStep,
    ControllerHost, ControllerHostAction, DecisionCommandPort, DefaultSettingsPort,
    DesktopNotificationPort, EnvironmentStorePort, Exit, ExternalTerminalPort, FixedBackendFactory,
    FsSessionWorktreeScanPort, GardenInputRoute, GardenInventoryPort, Geometry, GitDiff, IdleWatch,
    MAX_BACKGROUND_EXITS_PER_FRAME, MetricsPort, MetricsPortFactory, MissingWorkspacePrompt,
    NewStep, NoDesktopNotifications, NoMetrics, OpenStep, PROJECT_BAR_ROWS, PaneLaunch,
    PaneLaunchCommandPort, PrModalClickRoute, ProjectedSession, SerializedPaneLaunchPort,
    SessionCommandPort, SessionCommandPortFactory, SessionCommandResult, SessionLifecycle,
    SessionLifecycleProjection, SessionRefreshPort, SessionWorktreeHint, SessionWorktreeScanPort,
    Start, TerminalAttach, TerminalChunk, TerminalError, TerminalInputOutcome,
    TerminalInputResolution, TerminalSubscription, TerminalViewProjection,
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
    run_workspace_controller_with_backend_and_settings, run_workspace_deck, run_workspace_loading,
    safe_session_error, save_config_responsive, save_config_source_responsive,
    save_environment_responsive, save_setup_commands_responsive, select_right_pane_tab,
    select_root_terminal_tab, sidebar_pointer_event, step_config, step_new, step_open,
    step_workspace_config, terminal_geometry, visit_garden_agent, welcome_action,
    workspace_drawer_header_key, workspace_has_unsaved_surface, workspace_loading_visible,
    write_banner,
};
use crate::presentation::frame::TERMINAL_CURSOR_MARKER;
use crate::presentation::live_terminal::LiveTerminalControls;
use crate::presentation::views::config::AvailableAgentModels;
use crate::presentation::views::new::{Field, Mode, New};
use crate::presentation::views::open::Open;
use crate::presentation::views::welcome::MenuAction;
use crate::presentation::views::workspace::HomeProjection;
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
    Completions, DaemonBackend, DecisionPort as BackendDecisionPort, LaunchAgentRequest,
    ReopenAgentRequest,
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
use usagi_core::infrastructure::ipc::DaemonMetrics;

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

type SessionCommandCall = (String, Option<String>, SessionCommand);

struct RecordingExternalTerminalPort(Arc<Mutex<Vec<PathBuf>>>);

impl ExternalTerminalPort for RecordingExternalTerminalPort {
    fn open(&mut self, directory: &Path) -> Result<(), String> {
        self.0.lock().unwrap().push(directory.to_path_buf());
        Ok(())
    }
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
    use crate::usecase::application::daemon_backend::SessionLifecyclePort as _;

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

fn scoped_terminal_ref(workspace: WorkspaceId, session: Option<SessionId>) -> TerminalRef {
    TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: session,
        worktree_id: WorktreeId::new(),
    }
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

fn info() -> AppInfo {
    AppInfo {
        name: "usagi",
        version: "0.1.0",
    }
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

fn has_director_drawer(frames: &[Vec<String>]) -> bool {
    frames.iter().any(|frame| {
        let text = frame.join("\n");
        text.contains("♛ Director")
            && (text.contains("No conversations yet") || text.contains("Organization"))
            && text.contains("[ New ]")
    })
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

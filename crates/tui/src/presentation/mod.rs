//! TUI 面の presentation 層。画面描画（各画面の view・共通 widget）と
//! キー入力のマッピングを置く。描画は自前の差分レンダリングで行い、
//! UI フレームワーク（ratatui 等）には依存しない。
//! 実 IO は持たず、出力先は呼び出し側（合成ルート）から注入する。
//!
//! 描画は 3 つに分ける: 各画面の view（[`views`]）・再利用 UI 部品（[`widgets`]）・
//! 領域配置（[`layouts`]）。view が layout で領域を割り、そこへ widget を配置する。
//! 色は [`theme`] が意味的な役割で一元管理する（役割→具体色の単一情報源）。

mod banner;
mod config_setup;
mod controller_host;
mod director;
mod flow_steps;
pub mod frame;
mod frame_loop;
mod garden;
pub mod layouts;
pub mod live_terminal;
mod restore;
mod session_commands;
mod startup;
mod terminal_io;
pub mod theme;
pub mod views;
pub mod widgets;
mod work_run;
pub mod workspace_deck;
mod workspace_io;
pub mod workspace_runtime;
use frame_loop::drive_workspace_controller;
#[cfg(test)]
use frame_loop::{
    drain_controller_host_actions, home_frame_material, render_controller_frame,
    render_home_material,
};
pub use frame_loop::{render_home_snapshot, run_screen_graph_with_backend_and_notice};

use workspace_io::WorkspaceIoRuntime;

use terminal_io::{
    PaneLaunch, PaneLaunchCompletion, UnavailableExternalTerminalPort, UnavailablePaneLaunchPort,
    close_exited_panes, controller_terminal_view, drain_pane_completions_into_runtime,
    drain_pane_launches, enqueue_pane_launch, fail_terminal_launch, foreground_terminal_geometry,
    forward_live_terminal_input, intercept_live_terminal_control, live_action_to_app_key,
    managed_background_terminal, managed_background_terminal_geometry, select_right_pane_tab,
    sync_terminal_selection_motions, workspace_terminal_attachments,
};
#[cfg(test)]
use terminal_io::{
    PaneLaunchOutcome, close_focused_terminal_pane, copy_terminal_selection,
    handle_terminal_pointer, key_to_terminal_bytes, key_to_terminal_bytes_for_mode,
    poll_and_project_terminals, run_pane_launch, select_root_terminal_tab, terminal_geometry,
};

use flow_steps::{
    WelcomeStep, new_project_notice, save_config_responsive, step_config, step_new, step_open,
    step_welcome, step_workspace_config, unavailable_completion,
};
#[cfg(test)]
use flow_steps::{save_environment_responsive, unavailable_environment_error, welcome_action};

use session_commands::{
    SessionCommandCompletion, UnavailableSessionCommandPortFactory, begin_session_command,
    drain_session_completions, drain_session_refresh, project_controller_sessions,
    session_name_for, sync_runtime_sessions,
};
#[cfg(test)]
use session_commands::{
    UnavailableSessionCommandPort, apply_session_projection, emit_session_command_result,
    safe_session_error,
};

use restore::{
    RestoreRetryState, apply_restore_completion, restore_prepared_workspace,
    restore_workspace_closeup, restore_workspace_session_focus, spawn_restore_job,
};
#[cfg(test)]
use restore::{
    normalize_terminal_inventory, pane_restore_targets, restore_inventory_is_coherent,
    restore_open_panes,
};

use director::{
    apply_drawer_header_while_director_open, complete_director_launch, director_drawer_projection,
    handle_director_picker_input, is_director_new_click, is_director_new_pointer,
    open_director_from_new_button, select_director_tab_and_activate,
};
#[cfg(test)]
use director::{
    director_organization, select_director_agent, select_director_selection, select_director_tab,
};

use work_run::{
    UnavailableWorkRunPort, WorkRunControlInput, WorkRunLaneCompletion,
    handle_work_run_control_input_with_ui, spawn_work_run_control_job,
    spawn_work_run_observation_job, work_run_control_projection,
};
#[cfg(test)]
use work_run::{
    handle_work_run_control_input, handle_work_run_list_input, validate_work_run_snapshot,
};

#[cfg(test)]
use garden::GardenProjectVisit;
use garden::{
    GardenInputRoute, GardenObservationCompletion, UnavailableGardenInventoryPort,
    garden_shell_owned_wake, route_garden_input, spawn_garden_observation_job, visit_garden_agent,
};

pub use banner::{BannerScreenRunner, write_banner};
pub use controller_host::{ControllerHost, ControllerHostAction};
pub use startup::{StartupSplash, play_startup_splash};

#[cfg(test)]
use config_setup::save_setup_commands_responsive;
use config_setup::{save_config_source_responsive, step_setup_commands_editor};

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use chrono::{DateTime, Timelike, Utc};
use usagi_core::domain::agent::{
    AgentInventory, AgentProfileId, AgentResumeTarget, AgentRuntimeInventoryState,
    AgentWorkspaceObservation, ProviderResumeProjection,
};
use usagi_core::domain::id::{
    AgentContinuationRef, AgentRuntimeId, OperationId, RequestId, SessionId, TerminalRef,
    UserDecisionId, WorkspaceId,
};
use usagi_core::domain::recent::Recent;
use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
use usagi_core::domain::settings::{IconMode, WorkMode};
use usagi_core::domain::supervisor::{MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS, SupervisorRunId};
use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::domain::workspace::Workspace;
use usagi_core::usecase::env::EnvScope;
use usagi_core::usecase::vt_screen::RetainedRowMotion;

use crate::presentation::live_terminal::{LiveTerminalControls, PointerRelease};
use crate::presentation::theme::{Color, Style};
use crate::presentation::views::config::{self, AvailableAgentModels, Config};
use crate::presentation::views::director_drawer::{
    self, DirectorConversation, DirectorDrawerProjection, DirectorNewProjection,
    DirectorOrganizationRow, WorkRunControlProjection,
};
use crate::presentation::views::key_help::{self, Context as KeyHelpContext};
use crate::presentation::views::new::{DirectoryCompletion, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::pr_modal;
use crate::presentation::views::root_terminal_drawer;
use crate::presentation::views::welcome::{MenuAction, Welcome};
use crate::presentation::views::work_run::WorkRunProjection;
use crate::presentation::views::workspace::{
    self, HomeHeaderAction, HomeProjection, ProjectedSession, TerminalViewProjection,
    Workspace as WorkspaceView, garden_click_at, garden_fits, home_header_action_at, render_home,
    render_home_at, right_pane_tab_at, terminal_point_at,
};
use crate::presentation::widgets::modal::{self, ConfirmationView};
use crate::presentation::workspace_deck::{
    OverlayIntent, ProjectBarTarget, WorkspaceDeck, project_bar, render_overlay,
};
use crate::presentation::workspace_runtime::{
    InterruptedRemovalConfirmation, PaneRestoreTarget, WorkspaceRuntime,
};
use crate::usecase::application::agent_tab_intent::{
    AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabIntentPort,
    AgentTabIntentPortCommit, AgentTabProjection,
};
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, BackendEvent, BranchChoice, DecisionOverlayState,
    DirectorConsoleParent, DirectorNew, DirectorRoute, Effect, EnvironmentEntry, ExitChoice,
    Feedback, GardenClick, HomeMode, NewRequest, Notice, OperationResult, Overlay, PendingToken,
    PreviewFileFilter, Route, SessionBranchCatalog, SessionRoleCatalog, SessionRoleProjection,
    Target, WorkspaceDrawerFocus,
};
#[cfg(test)]
use crate::usecase::application::controller::{SafeError, SafeMessage};
use crate::usecase::application::daemon_backend::{
    Completions, DaemonBackend, DecisionPort as BackendDecisionPort, Flow as BackendFlow,
    OverlayPort as BackendOverlayPort, TargetStorePort as BackendTargetStorePort,
    WorkspaceCommandPort as BackendWorkspaceCommandPort,
};
use crate::usecase::application::interrupted_tab::{InterruptedTab, ResumeCommand};
use crate::usecase::application::metrics::{
    GitDiff, MetricsBackend, MetricsPort, MetricsPortFactory, MetricsProjection,
};
use crate::usecase::application::observation_lane::ObservationLane;
use crate::usecase::application::pane::{PaneKind, PaneRegistry, PaneTab, TabSelection};
use crate::usecase::application::pane_runtime::Geometry;
use crate::usecase::application::pr::{BrowserOpener, PrSnapshotPort};
use crate::usecase::application::terminal_screen::{PasteMode, TerminalBuffer, TerminalInputModes};
use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};
use crate::usecase::application::terminal_session::{
    SessionState, TerminalAttach, TerminalChunk, TerminalError, TerminalInputOutcome,
    TerminalInputResolution, TerminalSession, TerminalStreamPort, TerminalSubscription,
};
use crate::usecase::application::work_run_control::{
    WORK_RUN_ACTION_UNCONFIRMED, WorkRunControl, WorkRunControlAction, WorkRunControlError,
    WorkRunControlMode, WorkRunControlOutcome, WorkRunControlRequest, WorkRunControlResult,
    WorkRunPort,
};
use crate::usecase::application::{Key, Terminal, open_failure_notice};
use crate::usecase::overview::SessionCommand;
use crate::usecase::terminal_input::{
    LiveTerminalAction, PointerEvent, PointerKind, WHEEL_LINES, encode_mouse_wheel,
    encode_wheel_arrows,
};
use usagi_core::usecase::settings::SettingsPort;

#[cfg(test)]
use crate::usecase::application::WorkspaceCreateCompletion;
use crate::usecase::application::agent_runtime_ports::{
    AgentCommandPort, AgentCommandPortFactory, AgentPaneAdmission, ExactAgentResume,
    PaneLaunchCommandPort, SerializedPaneLaunchPort, TerminalCommandPort,
};
use crate::usecase::application::runtime_ports::{
    DecisionCommandPort, DesktopNotificationPort, EnvironmentStorePort, ExternalTerminalPort,
    GardenInventoryPort, RestoreConnectionPort, SessionBranchCatalogPort, SessionCatalogPort,
    SessionCommandPort, SessionCommandPortFactory, SessionCommandResult, SessionRefreshPort,
    SessionWorktreeScanPort,
};
use crate::usecase::application::{
    WorkspaceCreateEffect, WorkspaceCreateToken, WorkspaceLoader, WorkspaceSnapshot,
    runtime_identities_are_valid,
};

#[cfg(test)]
struct NoDesktopNotifications;
#[cfg(test)]
impl DesktopNotificationPort for NoDesktopNotifications {
    fn notify(&mut self, _: &str, _: &str) {}
}

/// Bridges the workspace [`AgentCommandPort`] into the [`TerminalStreamPort`]
/// expected by a [`TerminalSession`], so the session coordinator stays free of
/// the wider Agent launch vocabulary.
struct AgentStreamPort<'a, T: TerminalCommandPort + ?Sized>(&'a mut T);

impl<T: TerminalCommandPort + ?Sized> TerminalStreamPort for AgentStreamPort<'_, T> {
    fn connection_epoch(&self) -> Option<u64> {
        self.0.connection_epoch()
    }

    fn resize(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        self.0.resize(terminal, geometry)
    }

    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        self.0.attach(terminal, geometry)
    }
    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.0.poll(terminal, after_offset)
    }
    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        self.0
            .input(terminal, subscription, input_seq, operation, bytes)
    }
    fn input_outcome(
        &mut self,
        terminal: &TerminalRef,
        operation: OperationId,
        input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        self.0.input_outcome(terminal, operation, input_len)
    }
    fn detach(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription) {
        self.0.detach(terminal, subscription);
    }
}

/// The frontmost workspace surface that owns one input before PTY forwarding,
/// pane controls, and the Home reducer get a chance to observe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceForegroundInputOwner {
    /// The Director CLI picker, including its launch-pending projection, is an
    /// exclusive owner. Its small reserved-key vocabulary is reduced locally;
    /// every other user input is consumed inertly.
    DirectorPicker,
    /// No exclusive foreground surface owns the input at this routing seam.
    Downstream,
}

fn workspace_foreground_input_owner(runtime: &WorkspaceRuntime) -> WorkspaceForegroundInputOwner {
    if runtime.state().overlay().is_none()
        && runtime.state().director_drawer_open()
        && runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director)
        && (runtime.state().director_launching().is_some()
            || !matches!(runtime.state().director_new(), DirectorNew::Idle)
            || !matches!(runtime.state().director_route(), DirectorRoute::Console(_)))
    {
        WorkspaceForegroundInputOwner::DirectorPicker
    } else {
        WorkspaceForegroundInputOwner::Downstream
    }
}

/// Whether the drawer's selected root Agent, not the drawer itself, owns `Esc`.
///
/// An agent CLI reads `Esc` as its own interrupt / dismiss, so swallowing it to
/// close the drawer made that key unreachable for every conversation. The
/// drawer keeps `Esc` only when no live conversation can receive it — where
/// closing is the only thing left for it to mean — and `Ctrl-O Ctrl-G` still
/// closes the drawer with a live Agent attached.
fn drawer_agent_owns_escape(runtime: &WorkspaceRuntime) -> bool {
    runtime.wants_live_input() && runtime.focused_terminal().is_some()
}

/// Retarget `Ctrl-O` follow-ups whose meaning differs in an open drawer.
///
/// A root terminal drawer maps both `Ctrl-O n` and `Ctrl-O Ctrl-N` to a new
/// terminal tab. The classifier has already normalized the optional Control
/// modifier before this context-specific retargeting.
fn retarget_drawer_chords(runtime: &WorkspaceRuntime, key: Key) -> Key {
    if runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Terminal)
        && matches!(key, Key::Live(LiveTerminalAction::DirectorNew))
    {
        return Key::Live(LiveTerminalAction::NewRootTerminal);
    }
    key
}

#[derive(Debug, PartialEq, Eq)]
enum WorkspaceInputRoute {
    Drawer(Vec<Effect>),
    Garden(Vec<Effect>),
    Forwarded,
    Unhandled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrModalClickRoute {
    Inside,
    Outside,
}

fn route_pr_modal_click(
    overlay: Option<Overlay>,
    height: usize,
    width: usize,
    column: u16,
    row: u16,
) -> Option<PrModalClickRoute> {
    (overlay == Some(Overlay::Prs)).then(|| {
        if pr_modal::contains(height, width, column, row) {
            PrModalClickRoute::Inside
        } else {
            PrModalClickRoute::Outside
        }
    })
}

/// Give the PR modal ownership of a project-bar click before the process-level
/// bar can activate the surface behind it. Project-bar coordinates are outside
/// Home, so they cannot flow through [`route_pr_modal_click`]'s modal geometry.
fn dismiss_pr_modal_on_project_bar_click(runtime: &mut WorkspaceRuntime, key: &Key) -> bool {
    if runtime.state().overlay() != Some(Overlay::Prs) || !matches!(key, Key::Click { row: 0, .. })
    {
        return false;
    }
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));
    true
}

fn route_workspace_input_before_reducer(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    key: &Key,
) -> WorkspaceInputRoute {
    if let Some(effects) = handle_director_picker_input(runtime, key) {
        WorkspaceInputRoute::Drawer(effects)
    } else if forward_live_terminal_input(ui, runtime, controls, term, key) {
        WorkspaceInputRoute::Forwarded
    } else {
        WorkspaceInputRoute::Unhandled
    }
}

struct NoMetrics;
impl MetricsPort for NoMetrics {}

/// Complete production port set for one opened workspace.
pub struct ControllerBackendComposition {
    pub backend: DaemonBackend,
    /// Workspace-local role and Git ref discovery. Detached work receives a
    /// fresh worker adapter, so this resident port follows workspace teardown.
    pub session_catalogs: Box<dyn SessionCatalogPort>,
    pub session_commands: Box<dyn SessionCommandPort>,
    /// Resident session-inventory lane. It never shares the command port's
    /// connection, so a slow user-initiated create/remove and the background
    /// observation cannot block each other.
    pub session_refresh: Box<dyn SessionRefreshPort>,
    /// Resident terminal stream client. It stays with the live panes for the
    /// whole workspace and is never moved into a worker.
    pub agent_commands: Box<dyn AgentCommandPort>,
    /// Dedicated client shared by pane launch workers. It never shares the
    /// resident terminal stream connection.
    pub pane_launch_commands: Box<dyn PaneLaunchCommandPort>,
    /// Dedicated port moved into the off-thread restore job. It never shares
    /// the foreground terminal stream connection.
    pub restore_commands: Box<dyn AgentCommandPort>,
    /// Nonblocking, typed epochs from the dedicated restore connection. The
    /// controller drains this channel; it never probes daemon inventory from a
    /// frame tick.
    pub restore_connection: Box<dyn RestoreConnectionPort>,
    /// Dedicated port moved into the off-thread Garden observation job. It
    /// observes the *other* open projects' Agent inventory, so it never shares
    /// a connection with this workspace's own lanes.
    pub garden_inventory: Box<dyn GardenInventoryPort>,
    /// Dedicated serialized lane for daemon-owned `SupervisorRun` observation
    /// and human control. Serialization prevents an older snapshot from
    /// overtaking a control response in the UI.
    pub work_runs: Box<dyn WorkRunPort>,
    pub agent_tab_intents: Box<dyn AgentTabIntentPort>,
    pub external_terminal: Box<dyn ExternalTerminalPort>,
    pub metrics: Box<dyn MetricsPort>,
    pub browser: Box<dyn BrowserOpener>,
    /// Local worktree scan behind the inline create form's collision hint. It
    /// is a port so the frame loop's filesystem IO is countable in a test and
    /// stays out of the frame budget (#554).
    pub session_worktrees: Box<dyn SessionWorktreeScanPort>,
}

struct UnavailableRestoreConnectionPort;

impl RestoreConnectionPort for UnavailableRestoreConnectionPort {
    fn take_reconnected_epoch(&mut self) -> Option<u64> {
        None
    }
}

struct UnavailableSessionCatalogPort;

struct UnavailableSessionBranchCatalogPort;

impl SessionBranchCatalogPort for UnavailableSessionBranchCatalogPort {
    fn branches(&self, _: &Path, _: Option<&str>) -> SessionBranchCatalog {
        SessionBranchCatalog::default()
    }
}

impl SessionCatalogPort for UnavailableSessionCatalogPort {
    fn roles(&self, _: &Path) -> SessionRoleCatalog {
        SessionRoleCatalog::default()
    }

    fn branches(&self, _: &Path, _: Option<&str>) -> SessionBranchCatalog {
        SessionBranchCatalog::default()
    }

    fn branch_worker(&self) -> Box<dyn SessionBranchCatalogPort> {
        Box::new(UnavailableSessionBranchCatalogPort)
    }
}

/// Single factory used by direct launch and every screen-graph workspace entry.
pub trait ControllerBackendFactory {
    /// Process-level motion preference resolved by the composition root. Fakes
    /// keep full motion unless a test opts in explicitly.
    fn garden_reduced_motion(&self) -> bool {
        false
    }

    fn create(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition;
}

struct UnavailableBackendPort;

impl BackendTargetStorePort for UnavailableBackendPort {
    fn load_notes(&mut self, _: Target, completions: Completions) {
        unavailable_completion(&completions, "notes are unavailable");
    }
    fn save_notes(
        &mut self,
        _: Target,
        _: usagi_core::domain::note::Scratchpad,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "notes are unavailable");
    }
    fn load_environment(&mut self, _: EnvScope, completions: Completions) {
        unavailable_completion(&completions, "environment is unavailable");
    }
    fn save_environment(
        &mut self,
        _: EnvScope,
        _: Vec<EnvironmentEntry>,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "environment is unavailable");
    }
}

impl BackendWorkspaceCommandPort for UnavailableBackendPort {
    fn execute(
        &mut self,
        _: WorkspaceId,
        _: crate::usecase::overview::Command,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "workspace command is unavailable");
    }
}

impl BackendDecisionPort for UnavailableBackendPort {
    fn refresh(&mut self, _: WorkspaceId, completions: Completions) {
        unavailable_completion(&completions, "user decisions are unavailable");
    }
    fn resolve(
        &mut self,
        _: WorkspaceId,
        _: UserDecisionId,
        _: UserDecisionAnswer,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "user decisions are unavailable");
    }
}

impl BackendOverlayPort for UnavailableBackendPort {
    fn load_pull_requests(&mut self, _: Target, completions: Completions) {
        unavailable_completion(&completions, "Pull Request data is unavailable");
    }
    fn load_preview(
        &mut self,
        _: Target,
        _: RequestId,
        _: Option<String>,
        _: PreviewFileFilter,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "preview is unavailable");
    }
    fn open_pull_request(&mut self, _: String, completions: Completions) {
        unavailable_completion(&completions, "browser opening is unavailable");
    }
}

/// 対話ループが終了する理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// ユーザーが終了した（`q` / Ctrl-C、または起点画面で Esc）。プロセスを終える。
    Quit,
    /// 利用者が workspace を離れて Welcome へ戻ることを選んだ。プロセスは終わらない。
    ///
    /// 返すのは workspace 単体の runner だけである。screen graph はこの理由を自分の
    /// ループ内で `Screen::Welcome` へ解決するため、[`run_screen_graph_with_backend`]
    /// がこれを返すことはない（#556）。
    Welcome,
}

/// 対話ループの開始画面。合成ルートが `usagi`（Welcome）か `usagi config`（Config）かで選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// トップメニュー（Welcome）から始める。
    Welcome,
    /// 設定画面（Config）から始める。
    Config,
}

/// いま表示している画面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Welcome,
    Open,
    New,
    Config,
}

/// Config 画面でキー `key` を処理した結果の遷移。
enum ConfigStep {
    /// 同じ画面に留まる。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
    /// A save has begun (loading). The screen graph animates the Save button,
    /// writes, then on success holds the `done` frame before returning home; a
    /// failed write stays on Config with an error for retry.
    Save,
    /// A validated multiline editor write should be persisted by the caller.
    SaveSource,
}

/// Draw one complete highlight sweep across the pending Save button. Settings
/// writes are normally too quick for an intermediate state to be perceptible,
/// so the short, fixed sweep makes the transition visible before persistence.
fn play_config_save_wave(
    term: &mut dyn Terminal,
    form: &mut Config,
    base: Option<&[String]>,
) -> io::Result<()> {
    for frame in 0..config::SAVE_WAVE_FRAMES {
        let (height, width) = term.size()?;
        let lines = match base {
            Some(base) => config::render_over(height, width, base, form),
            None => config::render(height, width, form),
        };
        term.draw(&lines)?;
        if frame + 1 < config::SAVE_WAVE_FRAMES {
            term.wait(config::SAVE_WAVE_TICK)?;
            form.advance_save_animation();
        }
    }
    Ok(())
}

/// Workspace Config is a Home-owned modal and therefore cannot request that the
/// enclosing TUI exit. Quit chords are projected to [`Self::Stay`] at the modal
/// input boundary.
enum WorkspaceConfigStep {
    Stay,
    Back,
    Save,
    SaveSource,
}

/// New 画面でキー `key` を処理した結果の遷移。
enum NewStep {
    /// 同じ画面に留まる（フォーム編集を続ける）。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
    /// Ask the composition root to enumerate one directory's immediate children.
    CompleteDirectory(DirectoryCompletion),
    /// 検証済みの入力で workspace 作成を実行する。screen graph が backend を 1 回呼ぶ。
    Create(NewRequest),
}

/// One create admitted by the entry loop. `cancelled` is a navigation fence:
/// the worker may still finish, but its completion can no longer open a
/// workspace after the user leaves New.
struct PendingWorkspaceCreate {
    token: WorkspaceCreateToken,
    request: NewRequest,
    cancelled: bool,
}

/// A selected workspace path that disappeared before it could be opened.
/// The prompt owns only entry-screen state; the loader revalidates the paths
/// before any registry mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MissingWorkspacePrompt {
    paths: Vec<PathBuf>,
    confirmation: modal::ConfirmationModal,
}

impl MissingWorkspacePrompt {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            confirmation: modal::ConfirmationModal::new(),
        }
    }
}

/// Open 画面のキー処理結果。
enum OpenStep {
    Stay,
    Quit,
    Back,
    Choose(Vec<PathBuf>),
    ConfirmCleanup,
    ConfirmUnregister(PathBuf),
}

/// Workspace 画面のキー処理結果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkspaceStep {
    /// TUI を終了する。
    Quit,
    /// workspace を離れて Welcome へ戻る。呼び出し側がこの workspace のために
    /// 確立した資源を落としたあと、entry 画面を描き直す（#556）。
    Back,
    /// A target snapshot was prepared while the current composition was still
    /// resident. Returning drops that composition; the deck loop then commits
    /// the target and creates its replacement.
    Activate(Box<WorkspaceSnapshot>),
}

impl WorkspaceStep {
    /// workspace ループの停止理由を TUI 全体の終了理由へ投影する。workspace を
    /// 直接開いた入口（`usagi open <path>`）は Welcome を持たないため、合成ルートが
    /// [`Exit::Welcome`] を受けて screen graph へ入り直す。
    fn exit(self) -> Exit {
        match self {
            Self::Quit => Exit::Quit,
            Self::Back | Self::Activate(_) => Exit::Welcome,
        }
    }
}

struct WorkspaceConfigContext<'a> {
    settings: &'a mut dyn SettingsPort,
    available_models: AvailableAgentModels,
}

/// Effective defaults needed when entering one workspace.
///
/// Agent availability is observed by the composition root (the shell owns the
/// PATH probe); model and branch defaults come from effective settings. The
/// fallback fits callers without resolved settings: every CLI offered, `codex`
/// selected, and the current checkout used for new sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceEntryPolicy {
    available_models: AvailableAgentModels,
    default_model: usagi_core::domain::settings::DefaultModel,
    default_branch: Option<String>,
    work_mode: usagi_core::domain::settings::WorkMode,
    icon_mode: usagi_core::domain::settings::IconMode,
}

impl Default for WorkspaceEntryPolicy {
    fn default() -> Self {
        Self {
            available_models: AvailableAgentModels::all(),
            default_model: usagi_core::domain::settings::DefaultModel::default(),
            default_branch: None,
            work_mode: usagi_core::domain::settings::WorkMode::default(),
            icon_mode: usagi_core::domain::settings::IconMode::default(),
        }
    }
}

/// 既定では Agent launch を接続しない port。
///
/// daemon-backed Agent factory を注入しない screen-graph 経路（`run_with_settings`）で
/// controller ループを駆動するためのフォールバック。launch はインラインの失敗になり、
/// ローカルでプロセスを起動しない。
struct UnavailableAgentCommandPort;
impl AgentCommandPort for UnavailableAgentCommandPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent launch is unavailable.".to_owned())
    }
}

struct UnavailableAgentTabIntentPort;

impl AgentTabIntentPort for UnavailableAgentTabIntentPort {
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

/// Decision fallback for the screen-graph compatibility path. Production
/// composition injects its daemon-backed counterpart.
#[cfg(test)]
struct UnavailableDecisionCommandPort;
#[cfg(test)]
impl DecisionCommandPort for UnavailableDecisionCommandPort {
    fn refresh(&mut self, _workspace: WorkspaceId) -> BackendEvent {
        BackendEvent::Notice(Notice::new("User decisions are unavailable."))
    }

    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        _answer: UserDecisionAnswer,
    ) -> BackendEvent {
        BackendEvent::DecisionError {
            workspace,
            decision_id,
            error: SafeError {
                message: SafeMessage::new("User decisions are unavailable."),
                error_id: "decision-unavailable".to_owned(),
            },
        }
    }
}

/// Environment fallback for the screen-graph compatibility path and embedders
/// that inject no store. Production composition injects its state-backed
/// counterpart; this keeps the editor safe (it stays open, showing the error)
/// rather than silently discarding a load or save.
#[cfg(test)]
struct UnavailableEnvironmentStore;
#[cfg(test)]
impl EnvironmentStorePort for UnavailableEnvironmentStore {
    fn load(&mut self, scope: EnvScope) -> BackendEvent {
        BackendEvent::EnvironmentError {
            scope,
            error: unavailable_environment_error(),
        }
    }

    fn save(&mut self, scope: EnvScope, _entries: Vec<EnvironmentEntry>) -> BackendEvent {
        BackendEvent::EnvironmentError {
            scope,
            error: unavailable_environment_error(),
        }
    }
}

/// PR snapshot fallback for entry points that do not inject the daemon PR port
/// (the Welcome/Open/Recent screen graph). The PR overlay shows a safe notice.
#[cfg(test)]
struct UnavailablePrSnapshotPort;
#[cfg(test)]
impl PrSnapshotPort for UnavailablePrSnapshotPort {
    fn snapshot(
        &mut self,
        _session: SessionId,
    ) -> Result<usagi_core::infrastructure::ipc::PrSnapshot, String> {
        Err("Pull Request data is unavailable.".to_owned())
    }
}

/// Browser-open fallback for entry points that do not inject a platform opener.
struct UnavailableBrowserOpener;
impl BrowserOpener for UnavailableBrowserOpener {
    fn open(&mut self, _url: &str) -> Result<(), String> {
        Err("Browser opening is unavailable on this platform.".to_owned())
    }
}

/// The lane an embedder that injects no daemon-backed worker gets: it observes
/// nothing, so Home keeps the snapshot it opened with.
struct UnavailableSessionRefreshPort;

impl SessionRefreshPort for UnavailableSessionRefreshPort {
    fn wake(&mut self) {}

    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        None
    }
}

struct AgentTabIntentContext {
    workspace: WorkspaceId,
    allowed_sessions: BTreeSet<SessionId>,
    state: AgentTabIntent,
    port: Box<dyn AgentTabIntentPort>,
    /// Exact identities that were actually admitted to a runtime projection.
    /// Kept across a stale CAS observation so closing a still-visible O can
    /// dismiss its continuation while a fresh observation for R is in flight.
    visible_agents: Vec<(TerminalRef, AgentContinuationRef)>,
    load_error: Option<AgentTabIntentError>,
}

struct AgentTabObservation {
    projection: AgentTabProjection,
    cas_accepted: bool,
}

struct RestoreCompletion {
    port: Box<dyn AgentCommandPort>,
    dispatched_interaction: u64,
    dispatched_registry_revision: u64,
    dispatched_allowed_sessions: BTreeSet<SessionId>,
    terminals: Result<Vec<TerminalInventoryEntry>, TerminalError>,
    agents: Result<AgentInventory, String>,
    observation_coherent: bool,
}

struct RestoreApply {
    port: Box<dyn AgentCommandPort>,
    outcome: RestoreJobOutcome,
}

/// Steady cadence of the Garden's cross-project observation while the screen
/// saver is up. It bounds how stale another project's rabbits can be: cadence
/// plus one round of requests.
const GARDEN_OBSERVATION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1_000);

/// Cadence after a round that observed nothing (no daemon, refused workspace).
/// A Garden left open in front of a dead daemon must not retry every second.
const GARDEN_OBSERVATION_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5_000);

/// Most open projects observed in one round. Beyond this the extra tabs keep
/// their read-only plots rather than letting one round's request count follow an
/// unbounded tab list.
const MAX_OBSERVED_PROJECTS: usize = 16;

const WORK_RUN_OBSERVATION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2_000);
const WORK_RUN_OBSERVATION_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreJobOutcome {
    Applied,
    FenceRejected,
    TransportFailed,
    IntentFailed(AgentTabIntentError),
}

/// Background exits applied per frame. The observation lane queues them, so one
/// frame's tab-closing work stays bounded however many background tabs exited at
/// once; the rest are applied by the next frames.
const MAX_BACKGROUND_EXITS_PER_FRAME: usize = 8;
const DETACHED_TERMINAL_LIMIT: usize = 8;
/// The process-level project tab bar permanently owns the first terminal row.
const PROJECT_BAR_ROWS: usize = 1;
const WORKSPACE_SWITCH_LOADING_GRACE: std::time::Duration = std::time::Duration::from_millis(80);

const RESTORE_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(250);
const RESTORE_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreFollowup {
    None,
    ChangedObservation,
    Reconnected,
}

/// A create request in flight: the controller token used to reflux a failure and
/// the typed name shown in the sidebar's loading skeleton until the daemon's
/// `session.created` row replaces it.
struct PendingCreate {
    name: String,
}

struct AgentContext {
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    /// Resident terminal stream port. Attach, poll, input, resize, and detach
    /// keep using it for the whole workspace: no launch, resume, or restore
    /// worker ever takes it, so a slow daemon request cannot stop pane IO.
    port: Box<dyn AgentCommandPort>,
}

enum SessionBackendCompletion {
    Create {
        token: PendingToken,
        before: Vec<SessionId>,
        completions: Completions,
    },
    Remove {
        session: SessionId,
        before: Vec<SessionId>,
        completions: Completions,
    },
    Sleep {
        before: Vec<SessionId>,
        completions: Completions,
    },
}

/// Pending pane launches admitted beyond the one worker that owns the launch
/// client. The bound keeps a burst of activations from growing an unbounded queue
/// of pending tabs; a request past it completes as Busy instead of joining.
const PANE_LAUNCH_QUEUE_LIMIT: usize = 8;

/// Launch fence reserved for a completion no worker produced (an admission
/// refusal). Worker fences start at [`PANE_LAUNCH_FIRST`], so an unadmitted or
/// late completion can never free an admitted worker's slot.
const PANE_LAUNCH_UNADMITTED: u64 = 0;
const PANE_LAUNCH_FIRST: u64 = 1;

/// Safe feedback for a launch refused by bounded admission. The daemon never saw
/// the request, so retrying it is safe.
const PANE_LAUNCH_BUSY: &str = "too many pane launches are pending; try again";

/// Safe feedback for a worker that died inside the launch client. The request's
/// daemon effect is unknown, so the pane fails instead of being retried silently.
const PANE_LAUNCH_WORKER_FAILED: &str = "pane launch failed; check the daemon";

/// Run Config from an opened workspace. The form contains only workspace-owned
/// settings and returns to the still-live Home runtime after Escape or save.
fn run_workspace_config(
    term: &mut dyn Terminal,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
    branches: &[BranchChoice],
    base: &[String],
) -> io::Result<()> {
    let mut form = Config::load_workspace_with_available_models_and_branches(
        settings,
        available_models,
        branches,
    );
    let mut help = None;
    loop {
        let (height, width) = term.size()?;
        let frame = config::render_over(height, width, base, &form);
        let frame = if let Some(help) = help {
            key_help::render_over(height, width, &frame, help)
        } else {
            frame
        };
        term.draw(&frame)?;
        let key = term.read_key()?;
        if let Some(state) = help.as_mut() {
            if matches!(key, Key::Help | Key::Escape) {
                help = None;
            } else {
                let _ = scroll_key_help(state, &key, height);
            }
            continue;
        }
        if key == Key::Help {
            help = Some(key_help::State::new(
                config_help_context(&form),
                WorkMode::Classic,
            ));
            continue;
        }
        match step_workspace_config(&mut form, key, settings) {
            WorkspaceConfigStep::Stay => {}
            WorkspaceConfigStep::Back => return Ok(()),
            WorkspaceConfigStep::Save => {
                if save_config_responsive(term, &mut form, settings, Some(base))? {
                    let (height, width) = term.size()?;
                    term.draw(&config::render_over(height, width, base, &form))?;
                    term.wait(config::DONE_DISPLAY)?;
                    form.reset_save();
                    return Ok(());
                }
            }
            WorkspaceConfigStep::SaveSource => {
                let _ = save_config_source_responsive(term, &mut form, settings);
            }
        }
    }
}

fn config_help_context(config: &Config) -> KeyHelpContext {
    if config.is_selecting_team() {
        KeyHelpContext::TeamPicker
    } else if config.is_editing_setup_commands() {
        KeyHelpContext::SessionSetupEditor
    } else if config.is_editing_environment() {
        KeyHelpContext::EnvironmentEditor
    } else {
        KeyHelpContext::Config
    }
}

/// Translates a presentation [`Key`] into the controller's [`AppEvent`] vocabulary
/// for the real-terminal runtime that routes Home input through `update()`.
///
/// The composition-root adapter has already resolved the `Ctrl-O` live prefix, so
/// [`Key::Live`] arrives as a settled [`LiveTerminalAction`] that this function
/// maps to the equivalent [`AppKey`]. Ordinary keys map one-to-one; the reducer,
/// which owns overlay context, decides what each means. `Key::Other` and
/// `Key::Resize` (backend wakeups and terminal resizes the composition root
/// cannot express as input) advance the
/// mascot via [`AppEvent::Tick`] — real resize dimensions come from `term.size()`
/// and backend results from `DaemonBackend::drain_events()`, not from a `Key`.
///
/// Sidebar clicks need a monotonic timestamp and are adapted separately by
/// [`sidebar_pointer_event`]. Returns `None` for input the Home reducer never
/// consumes: raw PTY passthrough, pointer input, and keys with no Home management
/// meaning.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn app_event_from_key(key: Key) -> Option<AppEvent> {
    let app_key = match key {
        Key::Management { action, .. } => return Some(AppEvent::Key(action)),
        Key::Live(action) => return live_action_to_app_key(action).map(AppEvent::Key),
        Key::Resize | Key::Other => return Some(AppEvent::Tick),
        Key::Up => AppKey::Up,
        Key::Down => AppKey::Down,
        Key::PageUp => AppKey::PageUp,
        Key::PageDown => AppKey::PageDown,
        // Switch-mode Left/Right is consumed by the process-level project deck
        // before this mapping. When the keys reach the reducer they move a
        // horizontal choice such as the quit confirmation. Tab motion between
        // live tabs stays Ctrl-N/P.
        Key::Left => AppKey::Left,
        Key::Right => AppKey::Right,
        Key::Enter => AppKey::Enter,
        Key::Backspace => AppKey::Backspace,
        Key::Paste(text) => AppKey::Paste(text),
        Key::Tab => AppKey::Tab,
        Key::Escape => AppKey::Escape,
        // Runtime adapters preserve Ctrl-A as U+0001. `Ctrl-A` (LineStart) and
        // `Home` both mean `+ new session` here, where no text field owns focus:
        // the established sidebar-navigation contract that the reducer keeps intact.
        // A focused palette / create form intercepts these before the
        // reducer, so caret motion never reaches this navigation branch.
        Key::LineStart | Key::Home | Key::Char('\u{1}') => AppKey::CtrlA,
        Key::Char(character) => AppKey::Char(character),
        Key::Quit => AppKey::CtrlC,
        Key::CtrlQ => AppKey::CtrlQ,
        Key::CtrlX => AppKey::CtrlX,
        Key::Help => return None,
        Key::TerminalCopy { fallback } => {
            return {
                #[cfg(target_os = "windows")]
                {
                    let _ = fallback;
                    Some(AppEvent::Key(AppKey::CtrlC))
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = fallback;
                    None
                }
            };
        }
        // Input the Home reducer never consumes: raw PTY passthrough, terminal
        // pointer drags and clicks (a shell + `TerminalSession` concern), and the
        // caret/selection keys that have meaning only inside a focused text field
        // (End/Ctrl-E, Delete, Shift+arrows). Ctrl-D is terminal EOT and remains
        // inert on management surfaces.
        Key::Passthrough(_)
        | Key::Pointer(_)
        | Key::Click { .. }
        | Key::CtrlD
        | Key::End
        | Key::LineEnd
        | Key::Delete
        | Key::SelectLeft
        | Key::SelectRight
        | Key::SelectHome
        | Key::SelectEnd => {
            return None;
        }
    };
    Some(AppEvent::Key(app_key))
}

fn render_open(height: usize, width: usize, open: &Open, now: DateTime<Utc>) -> Vec<String> {
    let base = open::render(height, width, open, now);
    if let Some(path) = open.unregistering_path() {
        let title = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Unregister workspace");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint(&format!("Unregister {}?", path.display()));
        return modal::render_confirmation_over(
            height,
            width,
            &base,
            open.unregister_confirmation(),
            ConfirmationView::confirmation(
                &title,
                52,
                heading,
                "Only the registry entry is removed. Files stay.",
            ),
        );
    }
    // The cleanup prompt has no Yes/No focus toggle (y/Enter removes, n/Esc
    // cancels), so it flows through the shared confirmation renderer as a
    // compact, button-less variant. The state argument is unused when compact.
    if open.cleanup_confirming() {
        let title = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Clean up registry");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Remove missing registry entries?");
        return modal::render_confirmation_over(
            height,
            width,
            &base,
            modal::ConfirmationModal::new(),
            ConfirmationView::confirmation(
                &title,
                52,
                heading,
                "Registry entries whose folder is gone are removed.",
            )
            .compact("y: remove   n/Esc: cancel"),
        );
    }
    base
}

/// Ordered workspace paths represented by one Recent card.
fn recent_paths(recent: &Recent) -> Vec<PathBuf> {
    match recent {
        Recent::Workspace(overview) => vec![overview.workspace.path.clone()],
        Recent::Unite(unite) => unite
            .members()
            .iter()
            .map(|overview| overview.workspace.path.clone())
            .collect(),
    }
}

fn registry_contains_path(registry: &[Workspace], path: &Path) -> bool {
    for workspace in registry {
        if workspace.path == path {
            return true;
        }
    }
    false
}

fn remove_registry_paths(registry: &mut Vec<Workspace>, removed: &[PathBuf]) {
    let mut index = 0;
    while index < registry.len() {
        let mut matched = false;
        for path in removed {
            if registry[index].path == *path {
                matched = true;
                break;
            }
        }
        if matched {
            registry.remove(index);
        } else {
            index += 1;
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static SESSION_PROJECTION_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static TERMINAL_PROJECTION_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_projection_build_counts() {
    SESSION_PROJECTION_BUILDS.set(0);
    TERMINAL_PROJECTION_BUILDS.set(0);
}

#[cfg(test)]
fn projection_build_counts() -> (usize, usize) {
    (
        SESSION_PROJECTION_BUILDS.get(),
        TERMINAL_PROJECTION_BUILDS.get(),
    )
}

/// Test-only filesystem adapter. Production owns the equivalent adapter in the
/// binary composition root, outside this IO-free crate.
#[cfg(test)]
pub struct FsSessionWorktreeScanPort;

#[cfg(test)]
impl SessionWorktreeScanPort for FsSessionWorktreeScanPort {
    fn scan(&mut self, workspace: &Path) -> Vec<String> {
        let sessions = workspace.join(".usagi").join("sessions");
        std::fs::read_dir(sessions)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(std::fs::FileType::is_dir)
                    .map(|_| entry)
            })
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect()
    }
}

struct UnavailableSessionWorktreeScanPort;

impl SessionWorktreeScanPort for UnavailableSessionWorktreeScanPort {
    fn scan(&mut self, _: &Path) -> Vec<String> {
        Vec::new()
    }
}

/// Cadence gate that keeps the create form's collision hint off the frame
/// budget.
///
/// Before #554 the frame loop scanned `<workspace>/.usagi/sessions` on every
/// tick — about 62 `read_dir` calls plus one `stat` per entry every second,
/// growing with the session count, for a hint only the inline create form ever
/// reads. The scan now runs on the frame that opens the form and then at most
/// once per [`Self::CADENCE`] while it stays open; closing the form drops the
/// hint and stops the IO entirely.
///
/// Staleness is safe: the daemon re-checks the name when the request is
/// submitted and rejects a collision this hint missed, so the hint only has to
/// be good enough to catch the common case before the round trip.
struct SessionWorktreeHint {
    scan: Box<dyn SessionWorktreeScanPort>,
    names: Vec<String>,
    /// Elapsed time of the last scan, cleared whenever the form closes so the
    /// next opening always sees a freshly scanned hint.
    scanned_at: Option<std::time::Duration>,
}

impl SessionWorktreeHint {
    /// Ceiling on how often a form left open re-reads the directory.
    const CADENCE: std::time::Duration = std::time::Duration::from_millis(500);

    fn new(scan: Box<dyn SessionWorktreeScanPort>) -> Self {
        Self {
            scan,
            names: Vec::new(),
            scanned_at: None,
        }
    }

    /// The hint to fold into the reducer's advisory name copy this frame.
    ///
    /// Returns an empty slice while `form_open` is false, and never scans then.
    fn names(&mut self, form_open: bool, workspace: &Path, now: std::time::Duration) -> &[String] {
        if !form_open {
            self.names.clear();
            self.scanned_at = None;
            return &self.names;
        }
        let due = self
            .scanned_at
            .is_none_or(|last| now.saturating_sub(last) >= Self::CADENCE);
        if due {
            self.names = self.scan.scan(workspace);
            self.scanned_at = Some(now);
        }
        &self.names
    }
}

/// Commit one exact interrupted-lineage dismissal before removing its local
/// tab. The stable target protects modal confirmation from closing a different
/// tab if a background projection changed focus meanwhile.
fn dismiss_interrupted_history(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    target: Target,
    interrupted: InterruptedTab,
) {
    if let Err(error) = ui.mutate_agent_intent(AgentTabIntentMutation::DismissInterrupted {
        session_id: interrupted.session_id,
        continuation: interrupted.continuation,
        terminal: interrupted.last_terminal,
    }) {
        surface_agent_tab_intent_error(runtime, error);
        return;
    }
    runtime.dismiss_interrupted_tab(target, interrupted.continuation);
}

fn surface_agent_tab_intent_error(runtime: &mut WorkspaceRuntime, error: AgentTabIntentError) {
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(Notice::new(
        error.safe_message(),
    ))));
}

/// Resolve a persistent workspace-drawer button in the Home header that
/// produced the current frame.
///
/// This is resolved before drawer-local input ownership. In particular, the
/// Director picker deliberately consumes every other user input, but must not
/// make either visible drawer button inert.
fn workspace_drawer_header_key(key: &Key, width: usize, home: &HomeProjection) -> Option<AppKey> {
    let (column, row) = match key {
        Key::Click { column, row }
        | Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => (*column, *row),
        _ => return None,
    };
    match home_header_action_at(width, home, column, row) {
        Some(HomeHeaderAction::Director) => Some(AppKey::ToggleDirectorDrawer),
        Some(HomeHeaderAction::RootTerminal) => Some(AppKey::ToggleRootTerminalDrawer),
        _ => None,
    }
}

/// Focus the visible workspace drawer under a pointer press. Director is
/// tested first because it is composed last; a Shell remains clickable only in
/// the portion the right-side overlay does not cover.
fn focus_workspace_drawer_from_pointer(
    runtime: &mut WorkspaceRuntime,
    key: &Key,
    height: usize,
    width: usize,
) -> Option<WorkspaceDrawerFocus> {
    if runtime.state().overlay().is_some() {
        return None;
    }
    let (column, row) = match key {
        Key::Click { column, row }
        | Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => (usize::from(*column), usize::from(*row)),
        _ => return None,
    };
    let director = runtime.state().director_drawer_open().then(|| {
        let geometry = director_drawer::geometry(height, width);
        (geometry.left..geometry.left.saturating_add(geometry.width)).contains(&column)
            && (geometry.top..geometry.top.saturating_add(geometry.height)).contains(&row)
    });
    let focus = if director == Some(true) {
        Some(WorkspaceDrawerFocus::Director)
    } else if runtime.state().root_terminal_drawer_open() {
        let geometry = root_terminal_drawer::geometry_for_mode(
            height,
            width,
            workspace::root_terminal_available_width(
                height,
                width,
                runtime.state().director_drawer_open(),
            ),
            runtime.state().root_terminal_full_height(),
        );
        ((geometry.left..geometry.left.saturating_add(geometry.width)).contains(&column)
            && (geometry.top..geometry.top.saturating_add(geometry.height)).contains(&row))
        .then_some(WorkspaceDrawerFocus::Terminal)
    } else {
        None
    }?;
    let _ = runtime.apply_event(AppEvent::WorkspaceDrawerFocused(focus));
    Some(focus)
}

/// Everything the Home frame is a function of.
///
/// [`render_home_material`] is pure in this value, so the shell can compare it
/// against the material it last drew and skip both the frame build and
/// [`Terminal::draw`] when nothing changed (#554). Comparing the renderer's
/// inputs is what makes the skip safe, and it holds only because the renderer
/// reads nothing else — [`render_home_at`] takes even the wall clock as an
/// argument for that reason. A new renderer input belongs here too.
#[derive(Debug, PartialEq, Eq)]
struct HomeFrameMaterial {
    height: usize,
    width: usize,
    projection: HomeProjection,
    /// Safe label/reason and focused Remove/Keep answer for an unresumable
    /// interrupted conversation explicitly selected by the user.
    interrupted_removal_confirmation: Option<(String, String, bool)>,
    /// `Some(choice)` exactly while the exit prompt covers the frame, carrying
    /// the answer its focused button would commit (#556).
    quit_confirmation: Option<ExitChoice>,
    /// The create-failure dialog's safe message, present exactly while its
    /// overlay is open. Keying off the message avoids an unreachable "error
    /// overlay without a message" branch.
    create_error: Option<String>,
    /// The terminal-launch failure dialog's safe message.
    terminal_launch_error: Option<String>,
    /// The Agent-launch failure dialog's safe message.
    agent_launch_error: Option<String>,
    /// Failed-delete session label and focused Yes/No answer.
    force_remove_confirmation: Option<(String, bool)>,
    environment_editor: Option<crate::usecase::application::controller::EnvironmentEditor>,
    role_editor: Option<crate::usecase::application::controller::RoleEditor>,
    /// Minute-resolution wall clock behind relative session labels. Garden
    /// animation has a separate monotonic logical clock and never reads this.
    now: DateTime<Utc>,
}

impl HomeFrameMaterial {
    fn with_agent_inventory(
        mut self,
        inventory: Option<&AgentInventory>,
        panes: &PaneRegistry,
    ) -> Self {
        self.projection = self
            .projection
            .with_agent_inventory_and_panes(inventory, panes);
        self
    }

    fn with_work_runs(mut self, runs: WorkRunProjection) -> Self {
        self.projection = self.projection.with_work_runs(runs);
        self
    }

    fn with_garden_animation(mut self, tick: u64, reduced_motion: bool) -> Self {
        self.projection = self.projection.with_garden_reduced_motion(reduced_motion);
        self.projection = self
            .projection
            .with_garden_tick(self.height, self.width, tick);
        self
    }

    fn with_workspace_deck_garden(mut self, deck: &WorkspaceDeck) -> Self {
        let Some(active_sessions) = self.projection.garden_sessions().map(<[_]>::to_vec) else {
            return self;
        };
        let (scope, sessions) = deck.garden_projection(&active_sessions);
        self.projection = self.projection.with_deck_garden(scope, sessions);
        self
    }
}

fn relative_time_clock(now: DateTime<Utc>) -> DateTime<Utc> {
    now.with_second(0)
        .and_then(|now| now.with_nanosecond(0))
        .unwrap_or(now)
}

/// Add the project tab bar and deck overlay to a Home-height frame.
///
/// Both the ordinary frame loop and modal backgrounds use this composition so
/// opening a modal cannot move the workspace projection into row zero.
fn compose_workspace_shell_frame(
    deck: &WorkspaceDeck,
    home_height: usize,
    width: usize,
    home: &[String],
) -> Vec<String> {
    let mut frame = Vec::with_capacity(home_height.saturating_add(PROJECT_BAR_ROWS));
    frame.push(project_bar(deck, width).line);
    frame.extend(render_overlay(deck, home_height, width, home));
    frame
}

fn apply_agent_launch_completion(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    operation: OperationId,
    result: Result<AgentPaneAdmission, String>,
) {
    if result.is_ok() {
        // A cached pre-launch inventory must not filter an admitted runtime out.
        ui.request_agent_inventory_change_observation();
    }
    let Some(target) = pending_targets.remove(&operation) else {
        return;
    };
    let admission = match result {
        Ok(admission) => admission,
        Err(message) => {
            let _ = runtime.fail_pane(target, operation, message.clone());
            let _ = runtime.apply_event(AppEvent::AgentLaunchFailed(Notice::new(message)));
            complete_director_launch(runtime, target, operation, None, false);
            return;
        }
    };
    let supervisor_run_id = admission.supervisor_run_id;
    let terminal = admission.terminal;
    if let Some(continuation) = admission.continuation {
        let select =
            matches!(target, Target::Root(_)) || runtime.pane_completion_will_focus(operation);
        match ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
            session_id: target.session_id(),
            continuation,
            terminal: terminal.clone(),
            select,
        }) {
            Ok(()) => {
                let _ = runtime.complete_pane_focus_if_uninterrupted(target, operation, terminal);
            }
            Err(error) => {
                let _ = runtime.fail_pane(target, operation, error.safe_message().to_owned());
                surface_agent_tab_intent_error(runtime, error);
            }
        }
    } else if matches!(target, Target::Root(_)) && ui.agent_tab_intent.is_some() {
        let _ = runtime.fail_pane(
            target,
            operation,
            "daemon did not return a root Agent conversation".to_owned(),
        );
    } else {
        let _ = runtime.complete_pane_focus_if_uninterrupted(target, operation, terminal);
    }
    complete_director_launch(runtime, target, operation, supervisor_run_id, true);
}

/// Apply one explicit per-tab resume answer (#510).
///
/// The runtime validates the answer against the exact interrupted tab before any
/// tab changes; only an accepted replacement turns that one tab live, and its new
/// terminal is recorded as #506 display intent so the next observation keeps it.
/// Every refusal leaves the interrupted tab in place with safe feedback.
fn apply_exact_resume(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    target: Target,
    operation: OperationId,
    continuation: AgentContinuationRef,
    result: Result<ExactAgentResume, String>,
) {
    let resume = match result {
        Ok(resume) => resume,
        Err(message) => {
            runtime.fail_tab_resume_for(target, continuation, Some(operation), message);
            return;
        }
    };
    if let Err(rejection) = runtime.validate_tab_resume_for(
        target,
        continuation,
        operation,
        resume.continuation,
        resume.relation.as_ref(),
        &resume.terminal,
    ) {
        runtime.fail_tab_resume_for(
            target,
            continuation,
            Some(operation),
            rejection.safe_message().to_owned(),
        );
        return;
    }
    let session_id = target.session_id();
    if let Err(error) = ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
        session_id,
        continuation,
        terminal: resume.terminal.clone(),
        select: false,
    }) {
        runtime.fail_tab_resume_for(
            target,
            continuation,
            Some(operation),
            error.safe_message().to_owned(),
        );
        surface_agent_tab_intent_error(runtime, error);
        return;
    }
    let accepted = runtime.complete_tab_resume_for(
        target,
        continuation,
        operation,
        resume.continuation,
        resume.relation.as_ref(),
        &resume.terminal,
    );
    debug_assert!(accepted.is_ok(), "validated exact resume remains accepted");
}

/// Activate an interrupted tab after an explicit user selection. Passive
/// inventory restoration never calls this: a resumable lineage starts its
/// exact resume, while an unresumable lineage opens a destructive prompt.
fn activate_focused_interrupted_tab(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    if runtime.state().overlay().is_some() {
        return;
    }
    let Some(resumable) = runtime.focused_interrupted().map(InterruptedTab::resumable) else {
        return;
    };
    if resumable {
        resume_focused_interrupted_tab(ui, runtime, pending_targets);
    } else {
        let _ = runtime.open_interrupted_removal_confirmation();
    }
}

/// Give the unresumable-history prompt exclusive ownership of one key.
/// Persistence is committed before the exact tab leaves the registry; a failed
/// commit closes the prompt but leaves the history visible with a safe notice.
fn handle_interrupted_removal_confirmation(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    let Some(confirm_selected) = runtime
        .interrupted_removal_confirmation()
        .map(InterruptedRemovalConfirmation::is_confirm_selected)
    else {
        return false;
    };
    match key {
        Key::Left | Key::Right | Key::Tab => {
            runtime.toggle_interrupted_removal_choice();
        }
        Key::Char('y' | 'Y') => {
            confirm_interrupted_removal(ui, runtime);
        }
        Key::Enter => {
            if confirm_selected {
                confirm_interrupted_removal(ui, runtime);
            } else {
                let _ = runtime.take_interrupted_removal_confirmation();
            }
        }
        Key::Escape | Key::Char('n' | 'N') => {
            let _ = runtime.take_interrupted_removal_confirmation();
        }
        _ => {}
    }
    true
}

fn confirm_interrupted_removal(ui: &mut WorkspaceIoRuntime, runtime: &mut WorkspaceRuntime) {
    let Some(confirmation) = runtime.take_interrupted_removal_confirmation() else {
        return;
    };
    dismiss_interrupted_history(
        ui,
        runtime,
        confirmation.target(),
        confirmation.tab().clone(),
    );
}

/// Start an explicit resume of the selected interrupted tab (`Ctrl-O r` or a
/// direct tab selection).
///
/// This is the only path that asks the daemon to resume a provider conversation
/// per tab: the request carries the daemon's own opaque target plus a fresh
/// durable operation, and it marks exactly that tab pending.
fn resume_focused_interrupted_tab(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    let Some(workspace) = ui.agent.as_ref().map(|agent| agent.workspace) else {
        return;
    };
    let Some(target) = runtime.panes().active() else {
        return;
    };
    let Some(continuation) = runtime
        .focused_interrupted()
        .map(|interrupted| interrupted.continuation)
    else {
        return;
    };
    // A refusal (no trustworthy exact target, or a repeated activation whose
    // request is already in flight) is already the pane's own feedback and must
    // never reach the daemon as a second request.
    let Ok(ResumeCommand {
        target: resume_target,
        operation,
    }) = runtime.resume_selected_tab(OperationId::new())
    else {
        return;
    };
    debug_assert_eq!(resume_target.workspace_id, workspace);
    pending_targets.insert(operation, target);
    enqueue_pane_launch(
        ui,
        PaneLaunch::ResumeExact {
            operation,
            continuation,
            target: resume_target,
        },
    );
}

/// Build the controller event for a sidebar click. The shell supplies the raw
/// cell and an injected monotonic timestamp; stable identity and double-click
/// detection remain controller responsibilities.
fn sidebar_pointer_event(column: u16, row: u16, at: std::time::Duration) -> AppEvent {
    AppEvent::Pointer { column, row, at }
}

/// Whether a key read from the terminal is a *user* interaction.
///
/// [`Key::Other`] is how the composition root delivers a frame wake-up: an
/// animation tick, a drained daemon event, or terminal output arriving behind
/// the frame. None of those is a person touching the terminal, so none of them
/// postpones the screen saver — an Agent can work for an hour and the garden
/// still opens. Everything else in the vocabulary — keys, paste, pointer
/// presses and wheel, the OS copy shortcut, PTY passthrough, and a resize —
/// is an interaction and resets the idle clock.
const fn is_user_activity(key: &Key) -> bool {
    !matches!(key, Key::Other)
}

/// Workspace-local drafts are not deck state and must not be discarded by a
/// project activation.
fn workspace_has_unsaved_surface(runtime: &WorkspaceRuntime) -> bool {
    let state = runtime.state();
    state.create_session_form().is_some()
        || state.note_editor().is_some()
        || state.environment_editor().is_some()
        || state.role_editor().is_some()
}

/// Carry the active workspace's sidebar cursor across the composition teardown.
/// Only stable session rows are remembered; transient action rows remain local
/// to the controller that owns them.
fn remember_workspace_session_focus(deck: &mut WorkspaceDeck, state: &AppState) {
    if let crate::usecase::application::controller::Selection::Target(Target::Session(session)) =
        state.selected()
    {
        deck.remember_session_focus(state.workspace(), session);
    }
}

/// Run one blocking operation on a scoped worker while continuing to paint.
/// For cancellable workspace opens, Escape discards a late result; the
/// underlying call is allowed to settle safely before its borrowed port is
/// returned. Non-cancellable saves leave queued input untouched.
fn run_workspace_loading<T: Send>(
    term: &mut dyn Terminal,
    label: &str,
    cancellable: bool,
    operation: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    run_workspace_loading_with(
        term,
        label,
        cancellable,
        false,
        operation,
        |height, width, frame, status, _| {
            widgets::loading::loading_screen(width, height, frame, frame / 3, status)
        },
    )
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=blocking_operations_keep_painting_and_workspace_open_can_be_cancelled
fn run_workspace_loading_with<T: Send>(
    term: &mut dyn Terminal,
    label: &str,
    cancellable: bool,
    defer_progress: bool,
    operation: impl FnOnce() -> io::Result<T> + Send,
    mut render: impl FnMut(usize, usize, usize, &str, bool) -> Vec<String>,
) -> io::Result<T> {
    std::thread::scope(|scope| {
        let worker = scope.spawn(operation);
        let mut frame = 0_usize;
        let mut cancelled = false;
        let started = std::time::Instant::now();
        loop {
            let (height, width) = term.size()?;
            let status = if cancelled { "Cancelling…" } else { label };
            let show_progress =
                workspace_loading_visible(defer_progress, cancelled, started.elapsed());
            term.draw(&render(height, width, frame, status, show_progress))?;
            // Even an immediately completed operation gets one visible frame.
            // Without this fence, fast settings writes skipped feedback
            // entirely and made the Enter key appear to do nothing.
            if worker.is_finished() {
                break;
            }
            let tick = if defer_progress && !show_progress {
                std::time::Duration::from_millis(16)
            } else {
                std::time::Duration::from_millis(80)
            };
            if cancellable && !cancelled {
                if let Some(key) = term.wait_for_key(tick)? {
                    if matches!(key, Key::Escape) {
                        cancelled = true;
                    } else {
                        term.defer_key(key);
                    }
                }
            } else {
                // Saving and the post-cancel wait cannot accept input, but they
                // must still yield so a slow operation does not become a render
                // loop or consume keys intended for the next screen.
                term.wait(tick)?;
            }
            frame = frame.wrapping_add(1);
        }
        let result = worker
            .join()
            .map_err(|_| io::Error::other("background operation stopped unexpectedly"))?;
        if cancelled {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "operation was cancelled",
            ))
        } else {
            result
        }
    })
}

fn workspace_loading_visible(
    defer_progress: bool,
    cancelled: bool,
    elapsed: std::time::Duration,
) -> bool {
    cancelled || !defer_progress || elapsed >= WORKSPACE_SWITCH_LOADING_GRACE
}

fn cached_workspace_switch_frame(
    deck: &WorkspaceDeck,
    path: &Path,
    height: usize,
    width: usize,
    frame: usize,
    status: &str,
    show_progress: bool,
) -> Option<Vec<String>> {
    let slot = deck.slot_for_path(path)?;
    let sessions = slot.projected_sessions();
    let mut state = AppState::home(
        slot.workspace_id(),
        sessions.iter().map(|session| session.id).collect(),
    );
    if let Some(session) = deck.focused_session_for_path(path) {
        let _ = crate::usecase::application::controller::update(
            &mut state,
            AppEvent::FocusSession(session),
        );
    }
    let projection = HomeProjection::from_state(&state, slot.label(), &sessions)
        .with_icon_mode(deck.icon_mode());
    let projection = if show_progress {
        projection.with_content_loading(status, frame)
    } else {
        projection.with_content_pending()
    };
    let home_height = height.saturating_sub(PROJECT_BAR_ROWS);
    let home = render_home(home_height, width, &projection);
    let mut preview_deck = deck.clone();
    preview_deck.preview_path(path);
    Some(compose_workspace_shell_frame(
        &preview_deck,
        home_height,
        width,
        &home,
    ))
}

/// Open and refresh a workspace while an interactive production adapter keeps
/// presenting progress.
///
/// # Errors
///
/// Returns loader, terminal, worker, or cancellation failures.
pub fn open_workspace_responsive(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    path: &Path,
    label: &str,
) -> io::Result<WorkspaceSnapshot> {
    if !loader.background_operations() {
        let snapshot = loader.open(path)?;
        return Ok(refresh_empty_workspace_snapshot(loader, snapshot));
    }
    run_workspace_loading(term, label, true, || {
        let snapshot = loader.open(path)?;
        Ok(refresh_empty_workspace_snapshot(loader, snapshot))
    })
}

fn activate_workspace_responsive(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    path: &Path,
    label: &str,
) -> io::Result<()> {
    if loader.background_operations() {
        run_workspace_loading(term, label, false, || loader.activate_prepared(path))
    } else {
        loader.activate_prepared(path)
    }
}

fn activate_cached_workspace_responsive(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    deck: &WorkspaceDeck,
    path: &Path,
    label: &str,
) -> io::Result<()> {
    if !loader.background_operations() || !deck.contains_path(path) {
        return activate_workspace_responsive(term, loader, path, label);
    }
    run_workspace_loading_with(
        term,
        label,
        false,
        true,
        || loader.activate_prepared(path),
        |height, width, frame, status, show_progress| {
            cached_workspace_switch_frame(deck, path, height, width, frame, status, show_progress)
                .expect("the cached workspace path was checked before activation")
        },
    )
}

fn prepare_deck_workspace(
    term: &mut dyn Terminal,
    loader: &mut Option<&mut dyn WorkspaceLoader>,
    deck: &mut WorkspaceDeck,
    path: &Path,
    label: &str,
) -> Option<WorkspaceSnapshot> {
    let Some(loader) = loader.as_mut() else {
        deck.set_notice("Open the workspace list to add or switch projects.");
        return None;
    };
    let result = if (**loader).background_operations() && deck.contains_path(path) {
        run_workspace_loading_with(
            term,
            label,
            true,
            true,
            || {
                let snapshot = (**loader).open(path)?;
                Ok(refresh_empty_workspace_snapshot(&mut **loader, snapshot))
            },
            |height, width, frame, status, show_progress| {
                cached_workspace_switch_frame(
                    deck,
                    path,
                    height,
                    width,
                    frame,
                    status,
                    show_progress,
                )
                .expect("the cached workspace path was checked before opening")
            },
        )
    } else {
        open_workspace_responsive(term, &mut **loader, path, label)
    };
    match result {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            // A cancelled worker may already have completed its declaration.
            // Reassert the still-visible project before its resident pumps run
            // again, so cancellation never silently switches tenant authority.
            let current = deck.active_path().to_path_buf();
            let _ = activate_cached_workspace_responsive(
                term,
                &mut **loader,
                deck,
                &current,
                "Restoring current workspace…",
            );
            deck.set_notice(error.to_string());
            None
        }
    }
}

fn refresh_empty_workspace_snapshot(
    loader: &mut dyn WorkspaceLoader,
    snapshot: WorkspaceSnapshot,
) -> WorkspaceSnapshot {
    if snapshot.session_ids.is_empty() {
        let path = snapshot.workspace.path.clone();
        return loader.refresh(&path).unwrap_or(snapshot);
    }
    snapshot
}

fn prepare_activation_settings(
    workspace_config: &mut Option<WorkspaceConfigContext<'_>>,
    loader: &mut Option<&mut dyn WorkspaceLoader>,
    deck: &mut WorkspaceDeck,
    current: &Path,
    target: &Path,
) -> bool {
    let Some(context) = workspace_config.as_mut() else {
        return true;
    };
    if let Err(error) = context.settings.select_workspace(target) {
        let _ = context.settings.select_workspace(current);
        restore_prepared_workspace(loader, current);
        deck.set_notice(error.to_string());
        return false;
    }
    true
}

fn prepare_batch_settings(
    workspace_config: &mut Option<WorkspaceConfigContext<'_>>,
    loader: &mut Option<&mut dyn WorkspaceLoader>,
    deck: &mut WorkspaceDeck,
    current: &Path,
    prepared: &[WorkspaceSnapshot],
) -> bool {
    let Some(context) = workspace_config.as_mut() else {
        return true;
    };
    for snapshot in prepared {
        if let Err(error) = context.settings.select_workspace(&snapshot.workspace.path) {
            let _ = context.settings.select_workspace(current);
            restore_prepared_workspace(loader, current);
            deck.set_notice(error.to_string());
            return false;
        }
    }
    true
}

/// Row zero belongs to the project bar. Home continues to receive coordinates
/// relative to its own first row.
fn adjust_project_bar_pointer(key: Key) -> Key {
    match key {
        Key::Click { column, row } if row > 0 => Key::Click {
            column,
            row: row - 1,
        },
        Key::Pointer(mut pointer) if pointer.row > 0 => {
            pointer.row -= 1;
            Key::Pointer(pointer)
        }
        other => other,
    }
}

/// Tracks how long the user has been away from the keyboard.
///
/// The clock itself stays in the frame loop, exactly as it already does for
/// sidebar double-click detection: the shell reduces a monotonic [`Instant`] to
/// an elapsed [`Duration`] and injects it, so neither this unit nor the reducer
/// ever reads the wall clock.
///
/// [`Instant`]: std::time::Instant
#[derive(Debug)]
struct IdleWatch {
    /// Elapsed reading of the shell's monotonic clock at the last interaction.
    since: std::time::Duration,
}

impl IdleWatch {
    const fn new(now: std::time::Duration) -> Self {
        Self { since: now }
    }

    /// Observe one frame's key and return the idle duration to inject.
    fn observe(&mut self, key: &Key, now: std::time::Duration) -> std::time::Duration {
        if is_user_activity(key) {
            self.since = now;
        }
        now.saturating_sub(self.since)
    }
}

fn workspace_navigation_target(
    deck: &WorkspaceDeck,
    state: &AppState,
    key: &Key,
) -> Option<PathBuf> {
    if deck.overlay_open()
        || state.overlay().is_some()
        || state.director_drawer_open()
        || state.root_terminal_drawer_open()
    {
        return None;
    }
    match key {
        Key::Left if state.route() == Route::Home(HomeMode::Switch) => {
            Some(deck.previous_path().to_path_buf())
        }
        Key::Right if state.route() == Route::Home(HomeMode::Switch) => {
            Some(deck.next_path().to_path_buf())
        }
        Key::Live(LiveTerminalAction::PreviousWorkspace) => {
            Some(deck.previous_path().to_path_buf())
        }
        Key::Live(LiveTerminalAction::NextWorkspace) => Some(deck.next_path().to_path_buf()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceDeckHelp {
    None,
    AddWorkspace,
    WorkspaceFinder,
}

impl WorkspaceDeckHelp {
    const fn new(add_open: bool, any_overlay_open: bool) -> Self {
        if add_open {
            Self::AddWorkspace
        } else if any_overlay_open {
            Self::WorkspaceFinder
        } else {
            Self::None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceBaseHelp {
    Switch,
    Closeup,
    LiveTerminal,
}

impl WorkspaceBaseHelp {
    const fn new(route: Route, live_input: bool) -> Self {
        match route {
            Route::Home(HomeMode::Switch) => Self::Switch,
            Route::Home(HomeMode::Closeup) if live_input => Self::LiveTerminal,
            Route::Home(HomeMode::Closeup) => Self::Closeup,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceHelpState {
    deck: WorkspaceDeckHelp,
    overlay: Option<Overlay>,
    decision_answer_open: bool,
    work_run_mode: WorkRunControlMode,
    director_new_open: bool,
    director_route: DirectorRoute,
    drawer_focus: Option<WorkspaceDrawerFocus>,
    base: WorkspaceBaseHelp,
}

fn resolve_workspace_help_context(state: WorkspaceHelpState) -> KeyHelpContext {
    match state.deck {
        WorkspaceDeckHelp::AddWorkspace => return KeyHelpContext::AddWorkspace,
        WorkspaceDeckHelp::WorkspaceFinder => return KeyHelpContext::WorkspaceFinder,
        WorkspaceDeckHelp::None => {}
    }
    if let Some(overlay) = state.overlay {
        return match overlay {
            Overlay::Overview => KeyHelpContext::Overview,
            Overlay::Daemon => KeyHelpContext::Daemon,
            Overlay::Closeup => KeyHelpContext::CloseupActions,
            Overlay::QuitConfirmation => KeyHelpContext::ExitConfirmation,
            Overlay::ForceRemoveConfirmation => KeyHelpContext::ForceRemove,
            Overlay::Notes => KeyHelpContext::Scratchpad,
            Overlay::Environment => KeyHelpContext::WorkspaceEnvironmentEditor,
            Overlay::Roles => KeyHelpContext::RolesEditor,
            Overlay::CreateSession => KeyHelpContext::CreateSession,
            Overlay::Decisions if state.decision_answer_open => KeyHelpContext::DecisionAnswer,
            Overlay::Decisions => KeyHelpContext::DecisionList,
            Overlay::CleanupQueue => KeyHelpContext::CleanupQueue,
            Overlay::RemoveSessions => KeyHelpContext::RemoveSessions,
            Overlay::Prs => KeyHelpContext::PullRequests,
            Overlay::Preview => KeyHelpContext::Preview,
            Overlay::CreateSessionError => KeyHelpContext::CreateSessionError,
            Overlay::TerminalLaunchError => KeyHelpContext::TerminalLaunchError,
            Overlay::AgentLaunchError => KeyHelpContext::AgentLaunchError,
            Overlay::Garden => KeyHelpContext::Garden,
        };
    }
    if state.drawer_focus == Some(WorkspaceDrawerFocus::Director)
        && state.work_run_mode == WorkRunControlMode::Submitting
    {
        return KeyHelpContext::WorkRunSubmitting;
    }
    if state.drawer_focus == Some(WorkspaceDrawerFocus::Director)
        && matches!(
            state.director_route,
            DirectorRoute::WorkRuns | DirectorRoute::RunOverview(_)
        )
    {
        match state.work_run_mode {
            WorkRunControlMode::List if state.director_route == DirectorRoute::WorkRuns => {
                return KeyHelpContext::WorkRuns;
            }
            WorkRunControlMode::List => return KeyHelpContext::RunOverview,
            WorkRunControlMode::ResolveEscalation => return KeyHelpContext::WorkRunEscalation,
            WorkRunControlMode::ConfirmCancel
            | WorkRunControlMode::ConfirmDelete
            | WorkRunControlMode::Retry => return KeyHelpContext::WorkRunConfirmation,
            WorkRunControlMode::Submitting | WorkRunControlMode::Closed => {}
        }
    }
    if state.director_new_open && state.drawer_focus == Some(WorkspaceDrawerFocus::Director) {
        return KeyHelpContext::DirectorNew;
    }
    match state.drawer_focus {
        Some(WorkspaceDrawerFocus::Director) => {
            return match state.director_route {
                DirectorRoute::Organization => KeyHelpContext::Organization,
                DirectorRoute::WorkRuns => KeyHelpContext::WorkRuns,
                DirectorRoute::RunOverview(_) => KeyHelpContext::RunOverview,
                DirectorRoute::Console(DirectorConsoleParent::Organization) => {
                    KeyHelpContext::DirectorConsole
                }
                DirectorRoute::Console(DirectorConsoleParent::RunOverview(_)) => {
                    KeyHelpContext::WorkRunConsole
                }
            };
        }
        Some(WorkspaceDrawerFocus::Terminal) => return KeyHelpContext::RootShell,
        None => {}
    }
    match state.base {
        WorkspaceBaseHelp::Switch => KeyHelpContext::Switch,
        WorkspaceBaseHelp::Closeup => KeyHelpContext::Closeup,
        WorkspaceBaseHelp::LiveTerminal => KeyHelpContext::LiveTerminal,
    }
}

/// Resolve the frontmost visible surface before Help opens. The resulting
/// value is held for the lifetime of that Help overlay, so background daemon
/// updates cannot make the shortcut list jump while it is being read.
fn workspace_help_context(
    deck: &WorkspaceDeck,
    runtime: &WorkspaceRuntime,
    work_run_control: &WorkRunControl,
) -> KeyHelpContext {
    let state = runtime.state();
    resolve_workspace_help_context(WorkspaceHelpState {
        deck: WorkspaceDeckHelp::new(deck.add_overlay_open(), deck.overlay_open()),
        overlay: state.overlay(),
        decision_answer_open: state
            .decision_overlay()
            .and_then(DecisionOverlayState::editor)
            .is_some(),
        work_run_mode: work_run_control.mode(),
        director_new_open: state.director_launching().is_some()
            || state.director_new() != DirectorNew::Idle,
        director_route: state.director_route(),
        drawer_focus: state.workspace_drawer_focus(),
        base: WorkspaceBaseHelp::new(state.route(), runtime.wants_live_input()),
    })
}

/// Whether one workspace key opens the contextual keyboard-help overlay.
///
/// `Ctrl-?` remains global. Plain `?` is the discoverable shortcut on an
/// unobscured management Home, while a focused live terminal preserves plain
/// `?` for the PTY and uses the `Ctrl-O ?` action instead.
fn opens_workspace_help(
    key: &Key,
    deck: &WorkspaceDeck,
    runtime: &WorkspaceRuntime,
    work_run_control: &WorkRunControl,
) -> bool {
    match key {
        Key::Help | Key::Live(LiveTerminalAction::KeyboardHelp) => true,
        Key::Char('?') => {
            !deck.overlay_open()
                && runtime.state().overlay().is_none()
                && runtime.state().workspace_drawer_focus().is_none()
                && work_run_control.mode() == WorkRunControlMode::Closed
                && !runtime.wants_live_input()
        }
        _ => false,
    }
}

fn closes_workspace_help(key: &Key, context: KeyHelpContext) -> bool {
    matches!(
        key,
        Key::Help | Key::Escape | Key::Live(LiveTerminalAction::KeyboardHelp)
    ) || matches!(key, Key::Char('?'))
        && (context == KeyHelpContext::Switch || context == KeyHelpContext::Closeup)
}

fn scroll_key_help(state: &mut key_help::State, key: &Key, height: usize) -> bool {
    let page = height.saturating_sub(8).max(1);
    match key {
        Key::Up => state.scroll_up(1),
        Key::Down => state.scroll_down(1),
        Key::PageUp => state.scroll_up(page),
        Key::PageDown => state.scroll_down(page),
        Key::Home | Key::LineStart => state.scroll_home(),
        Key::End | Key::LineEnd => state.scroll_end(),
        _ => return false,
    }
    true
}

/// Maximum number of completions one Home frame may apply from each queue.
const FRAME_EVENT_BUDGET: usize = 128;

/// Registry reads are useful only while `+ Open` is visible. A short bounded
/// cadence feels live across processes without turning the Home frame loop into
/// a filesystem poller.
const REGISTRY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Run the controller-driven workspace runtime, mapping its stop to [`Exit`].
///
/// # Errors
///
/// Returns terminal IO failures from the interactive loop.
#[allow(clippy::too_many_arguments)]
pub fn run_workspace_controller_with_backend(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
) -> io::Result<Exit> {
    let mut deck = WorkspaceDeck::new(&snapshot);
    drive_workspace_controller(
        term,
        snapshot,
        &mut deck,
        &[],
        None,
        backend_factory,
        usagi_core::domain::settings::ModalSelectionMode::Action,
        usagi_core::domain::settings::PrAutoOpen::default(),
        WorkspaceEntryPolicy::default(),
        None,
    )
    .map(WorkspaceStep::exit)
}

/// Run a direct workspace entry with settings already resolved for that
/// workspace identity.
///
/// # Errors
///
/// Returns terminal IO failures from the interactive loop.
pub fn run_workspace_controller_with_backend_and_settings(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
    settings: &usagi_core::domain::settings::Settings,
) -> io::Result<Exit> {
    let mut deck = WorkspaceDeck::new(&snapshot);
    drive_workspace_controller(
        term,
        snapshot,
        &mut deck,
        &[],
        None,
        backend_factory,
        settings.modal_selection_mode,
        settings.pr_auto_open,
        WorkspaceEntryPolicy {
            default_model: settings.default_model,
            default_branch: settings.default_branch.clone(),
            work_mode: settings.work_mode,
            icon_mode: settings.icon_mode,
            ..WorkspaceEntryPolicy::default()
        },
        None,
    )
    .map(WorkspaceStep::exit)
}

/// Run a direct workspace entry with a writable settings port for Overview's
/// workspace-local `config` command.
///
/// # Errors
///
/// Returns workspace binding or terminal IO failures.
pub fn run_workspace_controller_with_backend_and_config(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    settings.select_workspace(&snapshot.workspace.path)?;
    let effective = usagi_core::usecase::settings::read_for_workspace_entry(settings);
    let mut deck = WorkspaceDeck::new(&snapshot);
    drive_workspace_controller(
        term,
        snapshot,
        &mut deck,
        &[],
        None,
        backend_factory,
        effective.modal_selection_mode,
        effective.pr_auto_open,
        WorkspaceEntryPolicy {
            available_models,
            default_model: effective.default_model,
            default_branch: effective.default_branch.clone(),
            work_mode: effective.work_mode,
            icon_mode: effective.icon_mode,
        },
        Some(WorkspaceConfigContext {
            settings,
            available_models,
        }),
    )
    .map(WorkspaceStep::exit)
}

/// Run a direct workspace entry inside the same process-level deck used by the
/// Welcome/Open graph, so `Ctrl-O +` is available immediately.
///
/// # Errors
///
/// Returns workspace preparation, settings, persistence, or terminal IO
/// failures.
#[allow(clippy::too_many_arguments)]
pub fn run_workspace_deck_with_backend_and_config(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    backend_factory: &mut dyn ControllerBackendFactory,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    enter_workspace(
        term,
        snapshot,
        registry,
        loader,
        settings,
        backend_factory,
        available_models,
    )
    .map(|exit| exit.unwrap_or(Exit::Welcome))
}

struct FixedBackendFactory {
    sessions: Option<Box<dyn SessionCommandPort>>,
    agent: Option<Box<dyn AgentCommandPort>>,
    launch: Option<Box<dyn PaneLaunchCommandPort>>,
    restore: Option<Box<dyn AgentCommandPort>>,
    metrics: Option<Box<dyn MetricsPort>>,
    browser: Option<Box<dyn BrowserOpener>>,
    /// Resident session-inventory lane injected as a fake by the frame-loop
    /// tests; unset means the workspace observes nothing (#551).
    session_refresh: Option<Box<dyn SessionRefreshPort>>,
    /// Decision lane injected as a fake by the frame-loop tests; unset keeps the
    /// unavailable port.
    decisions: Option<Box<dyn BackendDecisionPort>>,
    /// Worktree scan injected as a counting fake by the frame-loop tests; unset
    /// keeps the real `read_dir` (#554).
    session_worktrees: Option<Box<dyn SessionWorktreeScanPort>>,
}

impl ControllerBackendFactory for FixedBackendFactory {
    fn create(
        &mut self,
        _: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition {
        ControllerBackendComposition {
            backend: DaemonBackend::new(
                Box::new(host.clone()),
                Box::new(host),
                Box::new(UnavailableBackendPort),
                Box::new(UnavailableBackendPort),
            )
            .with_decisions(
                self.decisions
                    .take()
                    .unwrap_or_else(|| Box::new(UnavailableBackendPort)),
            )
            .with_overlay(Box::new(UnavailableBackendPort)),
            session_catalogs: Box::new(UnavailableSessionCatalogPort),
            session_commands: self
                .sessions
                .take()
                .expect("fixed session port is created once"),
            session_refresh: self
                .session_refresh
                .take()
                .unwrap_or_else(|| Box::new(UnavailableSessionRefreshPort)),
            agent_commands: self.agent.take().expect("fixed agent port is created once"),
            pane_launch_commands: self
                .launch
                .take()
                .unwrap_or_else(|| Box::new(UnavailablePaneLaunchPort)),
            restore_commands: self
                .restore
                .take()
                .unwrap_or_else(|| Box::new(UnavailableAgentCommandPort)),
            restore_connection: Box::new(UnavailableRestoreConnectionPort),
            garden_inventory: Box::new(UnavailableGardenInventoryPort),
            work_runs: Box::new(UnavailableWorkRunPort),
            agent_tab_intents: Box::new(UnavailableAgentTabIntentPort),
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            metrics: self
                .metrics
                .take()
                .expect("fixed metrics port is created once"),
            browser: self
                .browser
                .take()
                .expect("fixed browser port is created once"),
            session_worktrees: self
                .session_worktrees
                .take()
                .unwrap_or_else(|| Box::new(UnavailableSessionWorktreeScanPort)),
        }
    }
}

/// Compatibility entry for embedders that still supply individual host ports.
/// Production uses [`run_workspace_controller_with_backend`].
///
/// `agent_port` is the resident terminal stream client and `pane_launch_port` the
/// dedicated launch client: they are separate arguments because they must be
/// separate clients, so a slow launch cannot stop an existing pane's IO.
///
/// # Errors
///
/// Returns terminal IO failures from the interactive workspace loop.
#[allow(clippy::too_many_arguments)]
pub fn run_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
    agent_port: Box<dyn AgentCommandPort>,
    pane_launch_port: Box<dyn PaneLaunchCommandPort>,
    _decisions: Box<dyn DecisionCommandPort>,
    _environment: Box<dyn EnvironmentStorePort>,
    _desktop_notifications: Box<dyn DesktopNotificationPort>,
    metrics: Box<dyn MetricsPort>,
    _pr_port: Box<dyn PrSnapshotPort>,
    browser: Box<dyn BrowserOpener>,
) -> io::Result<Exit> {
    let mut factory = FixedBackendFactory {
        sessions: Some(session_commands),
        agent: Some(agent_port),
        launch: Some(pane_launch_port),
        restore: None,
        metrics: Some(metrics),
        browser: Some(browser),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };
    run_workspace_controller_with_backend(term, snapshot, &mut factory)
}

/// Open list 用に、registry の生値と recent projection を結び付ける。
///
/// `Recent::Workspace` は各登録 workspace の集計済み表示値を持つ。互換呼び出しで
/// projection が無いときだけ、生値から 0 件の overview を組み立てる。
fn open_from_registry(workspaces: Vec<Workspace>, recent: &[Recent]) -> Open {
    let open_overviews = recent
        .iter()
        .filter_map(|recent| match recent {
            Recent::Workspace(overview) => Some(overview.clone()),
            Recent::Unite(_) => None,
        })
        .collect::<Vec<_>>();
    if open_overviews.is_empty() && !workspaces.is_empty() {
        Open::new(workspaces)
    } else {
        Open::with_overviews(open_overviews)
    }
}

/// `start` で選んだ画面を起点にした対話 runtime。
///
/// Welcome→Open→Workspace と Welcome→Recent→Workspace は選択 path を同じ [`WorkspaceLoader`]
/// で開き、同じ Workspace runtime を駆動する。Workspace の基底 Switch では Esc は無効で、
/// Closeup や前面 modal を閉じるためだけに使う。workspace では `q` が TUI を閉じ、Ctrl-Q が
/// daemon-owned session を終了してから TUI を閉じる。
///
/// `workspaces` / `recent` / `now` は永続化・実時計を持つ呼び出し側から渡す。
///
/// # Errors
///
/// workspace の読み込み、端末への描画、キー読み取りのいずれかに失敗した場合、そのエラーを返す。
#[allow(clippy::too_many_arguments)] // screen data と注入 port（loader / settings / session port factory）を合成側から受ける入口。
pub fn run_with_settings(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
) -> io::Result<Exit> {
    run_with_settings_inner(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        None,
        None,
        AvailableAgentModels::all(),
    )
}

/// Run the screen graph with daemon Agent and metrics port factories.
///
/// # Errors
///
/// Returns workspace loading or terminal IO failures from the screen graph.
#[allow(clippy::too_many_arguments)]
pub fn run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
    agent_commands: &mut dyn AgentCommandPortFactory,
    available_models: AvailableAgentModels,
    metrics: &mut dyn MetricsPortFactory,
) -> io::Result<Exit> {
    run_with_settings_inner(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        Some(agent_commands),
        Some(metrics),
        available_models,
    )
}

/// Open one workspace snapshot through the controller runtime, supplying
/// fallback ports for the screen-graph entry points that do not inject a daemon
/// Agent / metrics factory (`run_with_settings`).
#[allow(clippy::too_many_arguments)]
fn open_snapshot_via_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    deck: &mut WorkspaceDeck,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<WorkspaceStep> {
    settings.select_workspace(&snapshot.workspace.path)?;
    let effective = usagi_core::usecase::settings::read_for_workspace_entry(settings);
    drive_workspace_controller(
        term,
        snapshot,
        deck,
        registry,
        Some(loader),
        backend_factory,
        effective.modal_selection_mode,
        effective.pr_auto_open,
        WorkspaceEntryPolicy {
            available_models,
            default_model: effective.default_model,
            default_branch: effective.default_branch.clone(),
            work_mode: effective.work_mode,
            icon_mode: effective.icon_mode,
        },
        Some(WorkspaceConfigContext {
            settings,
            available_models,
        }),
    )
}

/// Open one workspace through the controller runtime, then say where the screen
/// graph goes next.
///
/// `Some(exit)` means the TUI itself is finished; `None` means the workspace was
/// left for Welcome and the graph keeps running. Recent, Open, and New all route
/// through this one decision so leaving and quitting cannot diverge between the
/// three entries (#556).
fn enter_workspace(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<Option<Exit>> {
    let deck = WorkspaceDeck::new(&snapshot);
    enter_workspace_deck(
        term,
        snapshot,
        deck,
        registry,
        loader,
        settings,
        backend_factory,
        available_models,
    )
}

/// Run the process-level deck while keeping exactly one workspace composition
/// resident. A prepared activation returns from the old frame first, so all of
/// its ports are dropped before the next factory call.
#[allow(clippy::too_many_arguments)]
fn enter_workspace_deck(
    term: &mut dyn Terminal,
    mut snapshot: WorkspaceSnapshot,
    mut deck: WorkspaceDeck,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<Option<Exit>> {
    if deck.slots().len() > 1 {
        let _ = loader.record_unite(&deck.paths());
    }
    loop {
        let step = open_snapshot_via_controller(
            term,
            snapshot,
            &mut deck,
            registry,
            loader,
            settings,
            backend_factory,
            available_models,
        )?;
        match step {
            WorkspaceStep::Quit => return Ok(Some(Exit::Quit)),
            WorkspaceStep::Back => return Ok(None),
            WorkspaceStep::Activate(prepared) => {
                deck.activate_snapshot(&prepared);
                if deck.slots().len() > 1 {
                    let _ = loader.record_unite(&deck.paths());
                }
                snapshot = *prepared;
            }
        }
    }
}

fn prepare_workspace_deck(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    paths: &[PathBuf],
) -> io::Result<(Vec<WorkspaceSnapshot>, WorkspaceSnapshot, WorkspaceDeck)> {
    let Some((first_path, remaining_paths)) = paths.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a project deck needs at least one workspace",
        ));
    };
    let primary = open_workspace_responsive(term, loader, first_path, "Opening workspace 1…")?;
    let mut snapshots = vec![primary.clone()];
    for (index, path) in remaining_paths.iter().enumerate() {
        let label = format!("Opening workspace {} / {}…", index + 2, paths.len());
        snapshots.push(open_workspace_responsive(term, loader, path, &label)?);
    }
    activate_workspace_responsive(
        term,
        loader,
        &primary.workspace.path,
        "Activating workspace…",
    )?;
    let mut deck = WorkspaceDeck::new(&primary);
    deck.append_snapshots(&snapshots);
    Ok((snapshots, primary, deck))
}

struct CompatibilityBackendFactory<'a, 'b, 'c> {
    sessions: &'a mut dyn SessionCommandPortFactory,
    agents: Option<&'b mut dyn AgentCommandPortFactory>,
    metrics: Option<&'c mut dyn MetricsPortFactory>,
}

impl ControllerBackendFactory for CompatibilityBackendFactory<'_, '_, '_> {
    fn create(
        &mut self,
        _: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition {
        let agent_commands = self.agents.as_deref_mut().map_or_else(
            || -> Box<dyn AgentCommandPort> { Box::new(UnavailableAgentCommandPort) },
            AgentCommandPortFactory::create,
        );
        let metrics = self.metrics.as_deref_mut().map_or_else(
            || -> Box<dyn MetricsPort> { Box::new(NoMetrics) },
            MetricsPortFactory::create,
        );
        let backend = DaemonBackend::new(
            Box::new(host.clone()),
            Box::new(host),
            Box::new(UnavailableBackendPort),
            Box::new(UnavailableBackendPort),
        )
        .with_decisions(Box::new(UnavailableBackendPort))
        .with_overlay(Box::new(UnavailableBackendPort));
        // Each role gets its own client from the factory: the resident stream,
        // the launch client, and the restore client never share an instance.
        let pane_launch_commands = self.agents.as_deref_mut().map_or_else(
            || -> Box<dyn PaneLaunchCommandPort> { Box::new(UnavailablePaneLaunchPort) },
            |factory| {
                Box::new(SerializedPaneLaunchPort::new(factory.create()))
                    as Box<dyn PaneLaunchCommandPort>
            },
        );
        ControllerBackendComposition {
            backend,
            session_catalogs: Box::new(UnavailableSessionCatalogPort),
            session_commands: self.sessions.create(),
            session_refresh: Box::new(UnavailableSessionRefreshPort),
            agent_commands,
            pane_launch_commands,
            restore_commands: self.agents.as_deref_mut().map_or_else(
                || -> Box<dyn AgentCommandPort> { Box::new(UnavailableAgentCommandPort) },
                AgentCommandPortFactory::create,
            ),
            restore_connection: Box::new(UnavailableRestoreConnectionPort),
            garden_inventory: Box::new(UnavailableGardenInventoryPort),
            work_runs: Box::new(UnavailableWorkRunPort),
            agent_tab_intents: Box::new(UnavailableAgentTabIntentPort),
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            metrics,
            browser: Box::new(UnavailableBrowserOpener),
            session_worktrees: Box::new(UnavailableSessionWorktreeScanPort),
        }
    }
}

// The screen graph is an IO composition boundary.  Its choices are covered by
// the injected loader/port tests; LLVM coverage excludes only this terminal
// loop, consistently with the existing `run_with_settings` entry point.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_with_settings_inner(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
    mut agent_commands: Option<&mut dyn AgentCommandPortFactory>,
    mut metrics: Option<&mut dyn MetricsPortFactory>,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    let mut backend_factory = CompatibilityBackendFactory {
        sessions: session_commands,
        agents: agent_commands.take(),
        metrics: metrics.take(),
    };
    run_screen_graph_with_backend(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        &mut backend_factory,
        available_models,
    )
}

#[derive(Debug, PartialEq, Eq)]
enum EntryForm {
    Welcome(Welcome),
    Open(Open),
    New(New),
    Config(Config),
}

fn entry_help_context(
    screen: Screen,
    open: &Open,
    config: &Config,
    missing_workspace: bool,
) -> KeyHelpContext {
    if missing_workspace {
        return KeyHelpContext::MissingWorkspace;
    }
    match screen {
        Screen::Welcome => KeyHelpContext::Welcome,
        Screen::Open if open.unregistering_path().is_some() => KeyHelpContext::OpenUnregister,
        Screen::Open if open.cleanup_confirming() => KeyHelpContext::OpenCleanup,
        Screen::Open => KeyHelpContext::Open,
        Screen::New => KeyHelpContext::New,
        Screen::Config => config_help_context(config),
    }
}

fn render_missing_workspace_prompt(
    height: usize,
    width: usize,
    base: &[String],
    prompt: &MissingWorkspacePrompt,
) -> Vec<String> {
    let heading = if prompt.paths.len() == 1 {
        prompt.paths[0].display().to_string()
    } else {
        format!(
            "{} workspace directories no longer exist",
            prompt.paths.len()
        )
    };
    let message = if prompt.paths.len() == 1 {
        "Remove its registry entry? No workspace data is deleted."
    } else {
        "Remove their registry entries? No workspace data is deleted."
    };
    let mut view = ConfirmationView::confirmation("Workspace not found", 64, heading, message);
    view.confirm_label = "remove";
    view.cancel_label = "cancel";
    view.hints = "Enter/y: remove   Esc/n: cancel   ←→/Tab: choose";
    modal::render_confirmation_over(height, width, base, prompt.confirmation, view)
}

/// Production screen graph entry. Every Welcome/Open/Recent/New path creates
/// its workspace runtime through the same backend factory as direct launch.
///
/// # Errors
///
/// Returns workspace loading, settings, or terminal IO failures.
#[allow(clippy::too_many_arguments)]
pub fn run_screen_graph_with_backend(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    run_screen_graph_with_backend_and_notice(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        backend_factory,
        available_models,
        None,
    )
}

/// Run the screen graph with transient default settings. Embedders that own a
/// settings backend should call [`run_with_settings`] and inject its port.
///
/// # Errors
///
/// Returns terminal or workspace loading errors from the screen graph.
pub fn run(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
) -> io::Result<Exit> {
    let mut settings = DefaultSettingsPort;
    let mut session_commands = UnavailableSessionCommandPortFactory;
    run_with_settings(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        &mut settings,
        &mut session_commands,
    )
}

struct DefaultSettingsPort;

impl SettingsPort for DefaultSettingsPort {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests;

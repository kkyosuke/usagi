//! TUI 面の presentation 層。画面描画（各画面の view・共通 widget）と
//! キー入力のマッピングを置く。描画は v1 と同じく自前の差分レンダリングで行い、
//! UI フレームワーク（ratatui 等）には依存しない方針を引き継ぐ。
//! 実 IO は持たず、出力先は呼び出し側（合成ルート）から注入する。
//!
//! 描画は 3 つに分ける: 各画面の view（[`views`]）・再利用 UI 部品（[`widgets`]）・
//! 領域配置（[`layouts`]）。view が layout で領域を割り、そこへ widget を配置する。
//! 色は [`theme`] が意味的な役割で一元管理する（役割→具体色の単一情報源）。

pub mod frame;
pub mod layouts;
pub mod live_terminal;
pub mod metrics;
pub mod theme;
pub mod views;
pub mod widgets;
pub mod workspace_runtime;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use chrono::{DateTime, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::{
    AgentInventory, AgentProfileId, AgentResumeTarget, AgentRuntimeInventoryState,
    ProviderResumeProjection,
};
use usagi_core::domain::id::{
    AgentContinuationRef, OperationId, SessionId, TerminalRef, UserDecisionId, WorkspaceId,
};
use usagi_core::domain::recent::Recent;
use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::domain::workspace::Workspace;
use usagi_core::usecase::client::DaemonMetrics;

use crate::presentation::live_terminal::LiveTerminalControls;
use crate::presentation::metrics::{MetricsBackend, MetricsProjection};
use crate::presentation::theme::{Color, Style};
use crate::presentation::views::config::{self, AvailableAgentModels, Config};
use crate::presentation::views::create_session_error_modal;
use crate::presentation::views::new::{self, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::quit_modal;
use crate::presentation::views::splash;
use crate::presentation::views::welcome::{self, MenuAction, Welcome};
use crate::presentation::views::workspace::{
    self, GitDiff, HomeProjection, ProjectedSession, TerminalViewProjection,
    Workspace as WorkspaceView, render_home, terminal_point_at,
};
use crate::presentation::widgets::modal::{self, ConfirmationView};
use crate::presentation::workspace_runtime::{
    AgentReopenChoice, PaneRestoreTarget, WorkspaceRuntime,
};
use crate::usecase::application::agent_tab_intent::{
    AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabIntentPort,
    AgentTabIntentPortCommit, AgentTabProjection,
};
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, BackendEvent, Effect, EnvironmentEntry, Feedback, NewRequest,
    Notice, OperationResult, Overlay, PendingToken, Target,
};
#[cfg(test)]
use crate::usecase::application::controller::{SafeError, SafeMessage};
use crate::usecase::application::daemon_backend::{
    AgentPort as BackendAgentPort, Completions, CreateSessionRequest, DaemonBackend,
    DecisionPort as BackendDecisionPort, Flow as BackendFlow, LaunchAgentRequest,
    OpenTerminalRequest, OverlayPort as BackendOverlayPort, RemoveSessionRequest,
    ReopenAgentRequest, ResumeAgentRequest, SessionCommandPort as BackendSessionCommandPort,
    TargetStorePort as BackendTargetStorePort, WorkspaceCommandPort as BackendWorkspaceCommandPort,
};
use crate::usecase::application::pane::{PaneKind, PaneTab};
use crate::usecase::application::pane_runtime::Geometry;
use crate::usecase::application::pr::{BrowserOpener, PrSnapshotPort};
use crate::usecase::application::terminal_selection::TerminalSelection;
use crate::usecase::application::terminal_session::{
    SessionState, TerminalAttach, TerminalChunk, TerminalError, TerminalInputOutcome,
    TerminalSession, TerminalStreamPort,
};
use crate::usecase::application::{Key, ScreenRunner, Terminal};
use crate::usecase::overview::SessionCommand;
use crate::usecase::terminal_input::{LiveTerminalAction, PointerEvent, PointerKind};
use usagi_core::usecase::settings::SettingsPort;

pub use crate::usecase::application::{WorkspaceLoader, WorkspaceSnapshot};

/// Daemon-authoritative Agent launch boundary for the workspace runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaneAdmission {
    pub terminal: TerminalRef,
    pub continuation: Option<AgentContinuationRef>,
}

pub trait AgentCommandPort: Send {
    /// # Errors
    ///
    /// Returns a presentation-safe daemon launch failure.
    fn launch(
        &mut self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String>;

    /// Explicitly resumes retained provider-native metadata in a new daemon
    /// runtime. Implementations must not attach to the old PTY.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the resume or does not
    /// return a fully fenced terminal reference.
    fn resume(
        &mut self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation_id: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent resume is unavailable.".to_owned())
    }

    /// Returns the daemon's safe exact-target inventory for root and managed
    /// Agent histories in one workspace.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the workspace inventory
    /// request or returns an invalid projection.
    fn resume_inventory(&mut self, _workspace: WorkspaceId) -> Result<AgentInventory, String> {
        Err("Agent resume inventory is unavailable.".to_owned())
    }

    /// Resumes only the exact daemon-issued target selected by the caller.
    ///
    /// # Errors
    ///
    /// Returns safe feedback when the daemon rejects the exact target or does
    /// not return a fully fenced replacement terminal.
    fn resume_exact(
        &mut self,
        _target: AgentResumeTarget,
        _operation_id: OperationId,
    ) -> Result<TerminalRef, String> {
        Err("Exact Agent resume is unavailable.".to_owned())
    }

    /// Open a daemon-owned login shell for a scope. `session` is absent for the
    /// workspace root, whose checkout the daemon resolves to the trusted
    /// repository root.
    ///
    /// The default keeps embedders that only expose Agent launch safe: the
    /// Terminal action becomes an inline failure instead of spawning anything
    /// locally.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe launch failure.
    fn launch_terminal(
        &mut self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
    ) -> Result<TerminalRef, String> {
        Err("terminal launch is unavailable".to_owned())
    }

    /// Resize a daemon-owned terminal to the visible pane viewport.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn resize_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _geometry: Geometry,
    ) -> Result<(), TerminalError> {
        Ok(())
    }

    /// Attach to a daemon-owned terminal, taking its retained replay and cursor.
    ///
    /// The default keeps embedders without a terminal stream safe: attach fails
    /// and the pane shows only the tab, never a locally spawned process.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn attach_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Fetch the daemon terminal output produced after `after_offset`.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn poll_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Send input bytes to a daemon terminal, fenced by subscription/sequence.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication or terminal-ownership failure.
    fn input_terminal(
        &mut self,
        _terminal: &TerminalRef,
        _subscription: u64,
        _input_seq: u64,
        _bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Release a daemon terminal subscription; it must not stop the process.
    fn detach_terminal(&mut self, _terminal: &TerminalRef, _subscription: u64) {}

    /// List the daemon-owned runtimes in scope for this workspace so a freshly
    /// opened controller can re-project the terminals and Agents that are still
    /// live into pane tabs. The production adapter resolves the workspace root
    /// and every available session scope and unions the daemon inventory.
    ///
    /// The default keeps embedders without a daemon safe: no runtime is
    /// discovered, so opening a workspace simply starts with no restored panes.
    ///
    /// # Errors
    ///
    /// Returns a safe daemon communication failure; the caller then restores
    /// nothing rather than spawning anything locally.
    fn list_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, TerminalError> {
        Ok(Vec::new())
    }
}

/// Platform-native terminal launch boundary.
///
/// This is deliberately independent from [`AgentCommandPort`]: daemon terminal
/// streaming temporarily moves that port into a pane-launch worker, while
/// `terminal new` must remain available just as it is in v1.
pub trait ExternalTerminalPort: Send {
    /// Open a native terminal rooted at `directory`.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe platform launch failure.
    fn open(&mut self, directory: &Path) -> Result<(), String>;
}

/// Daemon-authoritative durable decision boundary for the workspace runtime.
///
/// The controller keeps the list and editor locally, while this port is the
/// only route that can refresh or resolve daemon-owned decisions.  Responses
/// are projected back through [`BackendEvent`], preserving the reducer's
/// one-way event flow and making the production adapter replaceable by a fake.
pub trait DecisionCommandPort: Send {
    /// Fetch the authoritative pending snapshot for one workspace.
    fn refresh(&mut self, workspace: WorkspaceId) -> BackendEvent;
    /// Submit one already validated answer. Rows remain visible until the
    /// returned confirmation event reaches the reducer.
    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
    ) -> BackendEvent;
}

/// Durable per-target environment boundary for the workspace runtime.
///
/// The controller keeps the editor's draft locally; this port is the only route
/// that reads and writes the persisted environment for a [`Target`] (workspace
/// root or session). Both operations project their result back through
/// [`BackendEvent`] (`EnvironmentLoaded` / `EnvironmentError`), preserving the
/// reducer's one-way event flow and keeping the editor's values through a save
/// failure. Resolving a target's stable identity to its store key is the
/// implementation's concern.
pub trait EnvironmentStorePort: Send {
    /// Read the persisted environment for `target`.
    fn load(&mut self, target: Target) -> BackendEvent;
    /// Persist the complete set of `entries` for `target`, replacing what was
    /// stored. On success the saved set refluxes as `EnvironmentLoaded`.
    fn save(&mut self, target: Target, entries: Vec<EnvironmentEntry>) -> BackendEvent;
}

/// Best-effort desktop-notification boundary for newly observed user decisions.
///
/// The TUI never depends on an OS command: unsupported platforms and delivery
/// failures are handled by the composition adapter, while the notice centre
/// remains usable.
pub trait DesktopNotificationPort {
    fn notify(&mut self, title: &str, body: &str);
}

#[cfg(test)]
struct NoDesktopNotifications;
#[cfg(test)]
impl DesktopNotificationPort for NoDesktopNotifications {
    fn notify(&mut self, _: &str, _: &str) {}
}

/// Bridges the workspace [`AgentCommandPort`] into the [`TerminalStreamPort`]
/// expected by a [`TerminalSession`], so the session coordinator stays free of
/// the wider Agent launch vocabulary.
struct AgentStreamPort<'a>(&'a mut dyn AgentCommandPort);

impl TerminalStreamPort for AgentStreamPort<'_> {
    fn resize(&mut self, terminal: &TerminalRef, geometry: Geometry) -> Result<(), TerminalError> {
        self.0.resize_terminal(terminal, geometry)
    }

    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        self.0.attach_terminal(terminal, geometry)
    }
    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.0.poll_terminal(terminal, after_offset)
    }
    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: u64,
        input_seq: u64,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        self.0
            .input_terminal(terminal, subscription, input_seq, bytes)
    }
    fn detach(&mut self, terminal: &TerminalRef, subscription: u64) {
        self.0.detach_terminal(terminal, subscription);
    }
}

/// Maps a management [`Key`] to the bytes a focused live terminal should
/// receive. Reserved prefix actions ([`Key::Live`]) do not reach the shell;
/// all other keys, including global controls, do while Closeup owns the pane.
fn key_to_terminal_bytes(key: Key) -> Option<Vec<u8>> {
    let bytes = match key {
        Key::Passthrough(bytes) => return (!bytes.is_empty()).then(|| bytes.clone()),
        // Forward a paste as one bracketed-paste block so an agent that requested
        // the mode inserts the multi-line text instead of submitting on every
        // embedded newline (the fix for pasting clipboard into the agent).
        Key::Paste(text) => {
            return (!text.is_empty())
                .then(|| crate::usecase::terminal_input::encode_bracketed_paste(&text));
        }
        Key::Char(ch) => ch.to_string().into_bytes(),
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::Right | Key::SelectRight => b"\x1b[C".to_vec(),
        Key::Left | Key::SelectLeft => b"\x1b[D".to_vec(),
        // The focused shell owns its own line editing: forward Home/Ctrl-A and
        // End/Ctrl-E as the readline control chords the previous mapping sent, so
        // caret keys that mean selection to a text field keep moving in the shell.
        Key::Home | Key::LineStart | Key::SelectHome => vec![1],
        Key::End | Key::LineEnd | Key::SelectEnd => vec![5],
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Quit => vec![3],
        Key::CtrlQ => vec![17],
        Key::CtrlD => vec![4],
        Key::Live(_)
        | Key::TerminalCopy { .. }
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Other => {
            return None;
        }
    };
    Some(bytes)
}

/// Forward one ordinary key to the focused Closeup terminal. Returns `true`
/// when the live pane owned the key, including the busy/error case where the
/// keystroke could not be delivered and a safe notice was recorded.
fn forward_live_terminal_input(
    ui: &mut WorkspaceUi,
    runtime: &WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    key: &Key,
) -> bool {
    if let Key::TerminalCopy { fallback } = key {
        let Some(terminal) = runtime
            .wants_live_input()
            .then(|| runtime.focused_terminal())
            .flatten()
        else {
            return false;
        };
        if controls.has_selection() {
            copy_terminal_selection(controls, term);
        } else if fallback.is_empty() {
            controls.set_feedback("no terminal text is selected");
        } else if let Err(message) = ui.send_terminal_bytes(&terminal, fallback) {
            controls.set_feedback(message);
        }
        return true;
    }
    let Some((terminal, bytes)) = runtime
        .wants_live_input()
        .then(|| runtime.focused_terminal())
        .flatten()
        .zip(key_to_terminal_bytes(key.clone()))
    else {
        return false;
    };
    // A launch worker temporarily owns the port; surface the dropped
    // keystroke instead of swallowing it silently.
    if let Err(message) = ui.send_terminal_bytes(&terminal, &bytes) {
        controls.set_feedback(message);
    }
    true
}

/// Pulls the latest safe daemon observation at a TUI redraw boundary.
pub trait MetricsPort {
    fn latest(&mut self) -> Option<DaemonMetrics>;

    /// Poll non-blocking session Git observations. The port owns any workers;
    /// rendering only receives completed values.
    fn git_diffs(&mut self, _sessions: &[(SessionId, PathBuf)]) -> BTreeMap<SessionId, GitDiff> {
        BTreeMap::new()
    }
}

/// Creates a fresh metrics port for every workspace opened from the screen graph.
pub trait MetricsPortFactory {
    fn create(&mut self) -> Box<dyn MetricsPort>;
}

struct NoMetrics;
impl MetricsPort for NoMetrics {
    fn latest(&mut self) -> Option<DaemonMetrics> {
        None
    }
}

struct NoMetricsFactory;
impl MetricsPortFactory for NoMetricsFactory {
    fn create(&mut self) -> Box<dyn MetricsPort> {
        Box::new(NoMetrics)
    }
}

/// Workspace entry ごとに fresh daemon Agent launch port を作る factory。
pub trait AgentCommandPortFactory {
    fn create(&mut self) -> Box<dyn AgentCommandPort>;
}

/// Actions whose stateful host remains in the terminal loop while
/// [`DaemonBackend`] is the sole controller-effect dispatcher.
pub enum ControllerHostAction {
    Create(CreateSessionRequest, Completions),
    Refresh(WorkspaceId, Completions),
    Remove(RemoveSessionRequest, Completions),
    LaunchAgent(LaunchAgentRequest),
    ResumeAgent(ResumeAgentRequest),
    ReopenAgent(ReopenAgentRequest),
    OpenTerminal(OpenTerminalRequest),
    OpenExternalTerminal(Target),
    SelectTab(crate::usecase::application::controller::TabDirection),
}

/// Cloneable adapter handed to the production backend factory. It contains no
/// policy: each port call enqueues exactly one action for the terminal host.
#[derive(Clone)]
pub struct ControllerHost(Sender<ControllerHostAction>);

impl ControllerHost {
    /// Create the host adapter and the terminal loop's action receiver.
    #[must_use]
    pub fn channel() -> (Self, Receiver<ControllerHostAction>) {
        let (sender, receiver) = mpsc::channel();
        (Self(sender), receiver)
    }
}

impl BackendSessionCommandPort for ControllerHost {
    fn create(&mut self, request: CreateSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Create(request, completions))) = self
            .0
            .send(ControllerHostAction::Create(request, completions))
        {
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: request.token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new("session command host is unavailable")),
            }));
        }
    }

    fn refresh(&mut self, workspace: WorkspaceId, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Refresh(_, completions))) = self
            .0
            .send(ControllerHostAction::Refresh(workspace, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }

    fn remove(&mut self, request: RemoveSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Remove(_, completions))) = self
            .0
            .send(ControllerHostAction::Remove(request, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }
}

impl BackendAgentPort for ControllerHost {
    fn launch_agent(&mut self, request: LaunchAgentRequest) {
        let _ = self.0.send(ControllerHostAction::LaunchAgent(request));
    }

    fn resume_agent(&mut self, request: ResumeAgentRequest) {
        let _ = self.0.send(ControllerHostAction::ResumeAgent(request));
    }

    fn reopen_agent(&mut self, request: ReopenAgentRequest) {
        let _ = self.0.send(ControllerHostAction::ReopenAgent(request));
    }

    fn open_terminal(&mut self, request: OpenTerminalRequest) {
        let _ = self.0.send(ControllerHostAction::OpenTerminal(request));
    }

    fn open_external_terminal(&mut self, target: Target) {
        let _ = self
            .0
            .send(ControllerHostAction::OpenExternalTerminal(target));
    }

    fn select_tab(&mut self, direction: crate::usecase::application::controller::TabDirection) {
        let _ = self.0.send(ControllerHostAction::SelectTab(direction));
    }
}

/// Complete production port set for one opened workspace.
pub struct ControllerBackendComposition {
    pub backend: DaemonBackend,
    pub session_commands: Box<dyn SessionCommandPort>,
    pub agent_commands: Box<dyn AgentCommandPort>,
    /// Dedicated port moved into the off-thread restore job. It never shares
    /// the foreground terminal stream connection.
    pub restore_commands: Box<dyn AgentCommandPort>,
    /// Nonblocking, typed epochs from the dedicated restore connection. The
    /// controller drains this channel; it never probes daemon inventory from a
    /// frame tick.
    pub restore_connection: Box<dyn RestoreConnectionPort>,
    pub agent_tab_intents: Box<dyn AgentTabIntentPort>,
    pub external_terminal: Box<dyn ExternalTerminalPort>,
    pub metrics: Box<dyn MetricsPort>,
    pub browser: Box<dyn BrowserOpener>,
}

/// Dedicated restore-client connection lifecycle observed by the composition
/// root. Epochs are strictly monotonic; duplicate delivery is harmless.
pub trait RestoreConnectionPort: Send {
    fn take_reconnected_epoch(&mut self) -> Option<u64>;
}

struct UnavailableRestoreConnectionPort;

impl RestoreConnectionPort for UnavailableRestoreConnectionPort {
    fn take_reconnected_epoch(&mut self) -> Option<u64> {
        None
    }
}

/// Single factory used by direct launch and every screen-graph workspace entry.
pub trait ControllerBackendFactory {
    fn create(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition;
}

struct UnavailableBackendPort;

struct UnavailableExternalTerminalPort;

impl ExternalTerminalPort for UnavailableExternalTerminalPort {
    fn open(&mut self, _: &Path) -> Result<(), String> {
        Err("external terminal launch is unavailable".to_owned())
    }
}

fn unavailable_completion(completions: &Completions, message: &str) {
    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
        message,
    ))));
}

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
    fn load_environment(&mut self, _: Target, completions: Completions) {
        unavailable_completion(&completions, "environment is unavailable");
    }
    fn save_environment(&mut self, _: Target, _: Vec<EnvironmentEntry>, completions: Completions) {
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
    fn load_preview(&mut self, _: Target, completions: Completions) {
        unavailable_completion(&completions, "preview is unavailable");
    }
    fn open_pull_request(&mut self, _: String, completions: Completions) {
        unavailable_completion(&completions, "browser opening is unavailable");
    }
}

/// 起動バナーを `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_banner(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{}", info.describe())
}

/// 対話ループが終了する理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// ユーザーが終了した（`q` / Ctrl-C、または起点画面で Esc）。
    Quit,
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

/// welcome 画面のキー処理結果。
enum WelcomeStep {
    Stay,
    Quit,
    OpenList,
    /// Recent の単体 workspace を開く。
    OpenRecent(usize),
    /// New（新規 workspace 作成フォーム）へ進む。
    NewForm,
    /// Config（設定画面）へ進む。
    ConfigScreen,
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
}

/// New 画面でキー `key` を処理した結果の遷移。
enum NewStep {
    /// 同じ画面に留まる（フォーム編集を続ける）。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
    /// 検証済みの入力で workspace 作成を実行する。screen graph が backend を 1 回呼ぶ。
    Create(NewRequest),
}

/// Open 画面のキー処理結果。
enum OpenStep {
    Stay,
    Quit,
    Back,
    Choose(PathBuf),
    ConfirmCleanup,
    ConfirmUnregister(PathBuf),
}

/// Workspace 画面のキー処理結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceStep {
    Quit,
}

struct WorkspaceConfigContext<'a> {
    settings: &'a mut dyn SettingsPort,
    available_models: AvailableAgentModels,
}

/// Overview の session command を daemon 所有の lifecycle runner へ渡す境界。
///
/// TUI は session store や git worktree を直接操作しない。実行時の合成ルートが
/// daemon IPC client をこの port として注入し、テストは fake port で command と
/// target の対応だけを検証する。
pub trait SessionCommandPort: Send + Sync {
    /// Execute one parsed Overview session command for this workspace and its
    /// currently selected session, when the command requires one.
    ///
    /// # Errors
    ///
    /// Returns a safe message when the daemon cannot accept the request.
    fn execute(
        &self,
        _workspace: &usagi_core::domain::workspace::Workspace,
        _selected: Option<&usagi_core::domain::session::SessionRecord>,
        _command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        Err("session command port is not implemented".to_owned())
    }
}

/// Safe result of a daemon-owned session command.
///
/// `sessions` is a read-only projection of the daemon lifecycle snapshot.  It
/// is intentionally returned to the UI instead of being persisted through the
/// legacy workspace state store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCommandResult {
    /// Message for the Overview modal.
    pub message: String,
    /// Authoritative sidebar rows when the daemon supplied a fresh snapshot.
    pub sessions: Option<Vec<usagi_core::domain::session::SessionRecord>>,
    /// Stable daemon identities aligned with [`Self::sessions`].  A lifecycle
    /// refresh must carry these together so a session created during this TUI
    /// run can subsequently launch an Agent without falling back to a name.
    pub session_ids: Option<Vec<SessionId>>,
    /// Safe provider resume state keyed by the same stable session identities.
    pub agent_resumes: Option<BTreeMap<SessionId, ProviderResumeProjection>>,
    /// Monotonically increasing daemon lifecycle revision for this snapshot.
    /// The UI uses it to ignore a response that arrives after a newer command.
    pub revision: Option<u64>,
}

impl SessionCommandResult {
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            sessions: None,
            session_ids: None,
            agent_resumes: None,
            revision: None,
        }
    }
}

struct UnavailableSessionCommandPort;

impl SessionCommandPort for UnavailableSessionCommandPort {
    fn execute(
        &self,
        _workspace: &usagi_core::domain::workspace::Workspace,
        _selected: Option<&usagi_core::domain::session::SessionRecord>,
        _command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        Err("session commands are unavailable".to_owned())
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
    fn load(&mut self, target: Target) -> BackendEvent {
        BackendEvent::EnvironmentError {
            target,
            error: unavailable_environment_error(),
        }
    }

    fn save(&mut self, target: Target, _entries: Vec<EnvironmentEntry>) -> BackendEvent {
        BackendEvent::EnvironmentError {
            target,
            error: unavailable_environment_error(),
        }
    }
}

#[cfg(test)]
fn unavailable_environment_error() -> SafeError {
    SafeError {
        message: SafeMessage::new("Environment is unavailable."),
        error_id: "environment-unavailable".to_owned(),
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
    ) -> Result<usagi_core::usecase::client::PrSnapshot, String> {
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

/// Workspace 起動ごとに Overview の [`SessionCommandPort`] を新しく作る境界。
///
/// screen graph（Welcome→Open / Recent）は 1 ループで複数の workspace を順に開くため、
/// port を都度 fresh に生成して daemon の revision state を workspace 間で持ち越さない。
/// TUI は daemon を知らないので、合成ルートが daemon-backed factory を実装して注入し、
/// テストは fake factory を渡す。
pub trait SessionCommandPortFactory {
    /// Build a fresh session command port for one workspace launch.
    fn create(&mut self) -> Box<dyn SessionCommandPort>;
}

/// 既定では session command を接続しない factory。
///
/// daemon-backed port を注入しない embedder / テスト経路で使う。
struct UnavailableSessionCommandPortFactory;

impl SessionCommandPortFactory for UnavailableSessionCommandPortFactory {
    fn create(&mut self) -> Box<dyn SessionCommandPort> {
        Box::new(UnavailableSessionCommandPort)
    }
}

/// daemon IO transport that the controller runtime keeps alongside its
/// [`WorkspaceRuntime`]: the session-create worker, the daemon-authoritative
/// session cache ([`WorkspaceView`]), pane launch workers, and live terminal
/// streams. Daemon metrics / git diffs are refluxed separately through
/// [`metrics::MetricsBackend`]. Home row state, input, and rendering belong to
/// the controller (`AppState`/`render_home`), not here.
struct WorkspaceUi {
    workspace: WorkspaceView,
    /// Shared daemon boundary. Admission allows one lifecycle worker at a time;
    /// snapshot revisions additionally fence stale authoritative observations.
    session_commands: std::sync::Arc<dyn SessionCommandPort>,
    last_session_revision: u64,
    /// Non-sensitive interrupted/resume state received from the daemon.
    agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
    session_completions: Receiver<SessionCommandCompletion>,
    session_completion_sender: Sender<SessionCommandCompletion>,
    /// Monotonic fence for the one admitted session command. A delayed or
    /// synthetic completion can never return its port into a newer command.
    next_session_command: u64,
    active_session_command: Option<u64>,
    /// Session displayed as a removal skeleton until its daemon command returns.
    removing_session: Option<SessionId>,
    /// An in-flight create's controller token and the name drawn in its sidebar
    /// skeleton (`document/03-tui.md`). Its completion can reflux a failure to
    /// the reducer as an [`OperationResult`]. `Some` only while a create worker
    /// owns the admission slot, so the skeleton clears when its result lands.
    creating_session: Option<PendingCreate>,
    agent: Option<AgentContext>,
    external_terminal: Box<dyn ExternalTerminalPort>,
    pane_launches: Vec<PaneLaunch>,
    pane_completions: Receiver<PaneLaunchCompletion>,
    pane_completion_sender: Sender<PaneLaunchCompletion>,
    /// Live coordinator for the active target's selected foreground terminal.
    /// Background and unselected tabs retain only their stable pane identity.
    terminals: Vec<TerminalSession>,
    terminal_reconnected: bool,
    terminal_size: (usize, usize),
    agent_tab_intent: Option<AgentTabIntentContext>,
    /// A successful durable Reopen requests one fresh coherent daemon
    /// observation. It never projects from an inventory cached before a later
    /// pane admission.
    agent_observation_requested: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreJobOutcome {
    Applied,
    FenceRejected,
    TransportFailed,
    IntentFailed(AgentTabIntentError),
}

const RESTORE_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(250);
const RESTORE_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(4);

/// Controller-owned admission and backoff for the dedicated restore client.
/// Frame ticks only consult this clock; they never imply a reconnect or issue an
/// inventory RPC by themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreRetryState {
    in_flight: bool,
    failures: u32,
    next_retry_at: Option<std::time::Duration>,
    notice_emitted: bool,
    reconnect_pending: bool,
    last_reconnect_epoch: u64,
}

impl RestoreRetryState {
    fn new() -> Self {
        Self {
            in_flight: false,
            failures: 0,
            next_retry_at: Some(std::time::Duration::ZERO),
            notice_emitted: false,
            reconnect_pending: false,
            last_reconnect_epoch: 0,
        }
    }

    fn begin_if_due(&mut self, now: std::time::Duration) -> bool {
        if self.in_flight || self.next_retry_at.is_none_or(|due| now < due) {
            return false;
        }
        self.in_flight = true;
        self.next_retry_at = None;
        true
    }

    /// Request one coherent observation after a durable local mutation. An
    /// existing outage keeps its backoff and an in-flight observation already
    /// sees the daemon state needed by this display-only mutation.
    fn request_observation(&mut self, now: std::time::Duration) {
        if !self.in_flight && self.next_retry_at.is_none() {
            self.next_retry_at = Some(now);
        }
    }

    /// Complete one bounded worker job. Returns whether this outage epoch needs
    /// its one coalesced user notice.
    fn complete(&mut self, now: std::time::Duration, outcome: RestoreJobOutcome) -> bool {
        self.in_flight = false;
        if self.reconnect_pending {
            self.reconnect_pending = false;
            self.failures = 0;
            self.next_retry_at = Some(now);
            self.notice_emitted = false;
            return false;
        }
        match outcome {
            RestoreJobOutcome::Applied | RestoreJobOutcome::IntentFailed(_) => {
                self.failures = 0;
                self.next_retry_at = None;
                self.notice_emitted = false;
                return false;
            }
            // The inventory was observed under an obsolete interaction/revision
            // fence. Its dedicated port is already back, so immediately admit
            // one observation under the fresh fence. This is a UI race, not a
            // daemon outage: do not back off or emit an outage notice.
            RestoreJobOutcome::FenceRejected => {
                self.failures = 0;
                self.next_retry_at = Some(now);
                self.notice_emitted = false;
                return false;
            }
            RestoreJobOutcome::TransportFailed => {}
        }
        self.failures = self.failures.saturating_add(1);
        let shift = self.failures.saturating_sub(1).min(4);
        let delay = RESTORE_RETRY_BASE
            .checked_mul(1_u32 << shift)
            .unwrap_or(RESTORE_RETRY_MAX)
            .min(RESTORE_RETRY_MAX);
        self.next_retry_at = Some(now.saturating_add(delay));
        if self.notice_emitted {
            false
        } else {
            self.notice_emitted = true;
            true
        }
    }

    /// A typed connection-epoch transition schedules exactly one fresh
    /// observation. A transition racing an in-flight job is remembered until
    /// that job returns its dedicated port.
    fn reconnected(&mut self, epoch: u64, now: std::time::Duration) {
        if epoch <= self.last_reconnect_epoch {
            return;
        }
        self.last_reconnect_epoch = epoch;
        self.failures = 0;
        self.notice_emitted = false;
        if self.in_flight {
            self.reconnect_pending = true;
        } else {
            self.next_retry_at = Some(now);
        }
    }
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
    /// A launch worker temporarily owns this port. Terminal streaming resumes
    /// only after the worker returns it with the daemon result.
    port: Option<Box<dyn AgentCommandPort>>,
}

struct SessionCommandCompletion {
    command_id: u64,
    result: Result<SessionCommandResult, String>,
    completion: SessionBackendCompletion,
}

enum SessionBackendCompletion {
    Create {
        token: PendingToken,
        before: Vec<SessionId>,
        completions: Completions,
    },
    Refresh {
        before: Vec<SessionId>,
        completions: Completions,
    },
    Remove {
        session: SessionId,
        before: Vec<SessionId>,
        completions: Completions,
    },
}

/// Completion of one non-blocking Agent / terminal launch. Keeping the port in
/// the message mirrors session creation: one daemon client remains the owner
/// of its request sequence while the TUI continues rendering the wave.
struct PaneLaunchCompletion {
    port: Box<dyn AgentCommandPort>,
    outcome: PaneLaunchOutcome,
}

enum PaneLaunchOutcome {
    Agent {
        operation: OperationId,
        result: Result<AgentPaneAdmission, String>,
    },
    Terminal {
        operation: OperationId,
        result: Result<TerminalRef, String>,
    },
}

/// A pane has already been rendered as pending before this work is run.
enum PaneLaunch {
    Agent {
        operation: OperationId,
        workspace: WorkspaceId,
        /// Absent for a workspace-root Agent.
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
        resume: bool,
    },
    Terminal {
        operation: OperationId,
        workspace: WorkspaceId,
        /// Absent for a workspace-root terminal.
        session: Option<SessionId>,
        arguments: String,
    },
}

impl WorkspaceUi {
    fn new(workspace: WorkspaceView, session_commands: Box<dyn SessionCommandPort>) -> Self {
        let (session_completion_sender, session_completions) = mpsc::channel();
        let (pane_completion_sender, pane_completions) = mpsc::channel();
        Self {
            workspace,
            session_commands: std::sync::Arc::from(session_commands),
            last_session_revision: 0,
            agent_resumes: BTreeMap::new(),
            session_completions,
            session_completion_sender,
            next_session_command: 1,
            active_session_command: None,
            removing_session: None,
            creating_session: None,
            agent: None,
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            pane_launches: Vec::new(),
            pane_completions,
            pane_completion_sender,
            terminals: Vec::new(),
            terminal_reconnected: false,
            terminal_size: (0, 0),
            agent_tab_intent: None,
            agent_observation_requested: false,
        }
    }

    fn set_terminal_size(&mut self, height: usize, width: usize) {
        self.terminal_size = (height, width);
    }

    fn with_agent_context(
        mut self,
        workspace: WorkspaceId,
        sessions: Vec<SessionId>,
        port: Box<dyn AgentCommandPort>,
    ) -> Self {
        self.agent = Some(AgentContext {
            workspace,
            sessions,
            port: Some(port),
        });
        self
    }

    fn with_agent_tab_intent(
        mut self,
        workspace: WorkspaceId,
        allowed_sessions: BTreeSet<SessionId>,
        mut port: Box<dyn AgentTabIntentPort>,
    ) -> Self {
        let (state, load_error) = match port.load(workspace) {
            Ok(state) => (state, None),
            Err(error) => (AgentTabIntent::empty(workspace), Some(error)),
        };
        self.agent_tab_intent = Some(AgentTabIntentContext {
            workspace,
            allowed_sessions,
            state,
            port,
            visible_agents: Vec::new(),
            load_error,
        });
        self
    }

    fn take_agent_tab_intent_load_error(&mut self) -> Option<AgentTabIntentError> {
        self.agent_tab_intent
            .as_mut()
            .and_then(|context| context.load_error.take())
    }

    fn with_agent_resumes(
        mut self,
        agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
    ) -> Self {
        self.agent_resumes = agent_resumes;
        self
    }

    fn with_external_terminal(mut self, port: Box<dyn ExternalTerminalPort>) -> Self {
        self.external_terminal = port;
        self
    }

    /// Attach to a freshly launched daemon terminal and start streaming it.
    ///
    /// A failed attach still records the session so its safe feedback renders;
    /// it never spawns a local process.
    fn start_terminal_session(&mut self, terminal: TerminalRef, geometry: Geometry) {
        if self
            .terminals
            .iter()
            .any(|session| session.terminal().fences(&terminal))
        {
            return;
        }
        if let Some(port) = self
            .agent
            .as_mut()
            .and_then(|agent| agent.port.as_deref_mut())
        {
            let mut session = TerminalSession::new(terminal, geometry);
            session.connect(&mut AgentStreamPort(port));
            self.terminals.push(session);
        }
    }

    /// Keep exactly the active target's selected foreground terminal attached.
    /// Every background target and unselected tab remains detached.
    fn sync_foreground_terminal(&mut self, focused: Option<&TerminalRef>, geometry: Geometry) {
        let stale = self
            .terminals
            .iter()
            .filter(|session| focused.is_none_or(|terminal| !session.terminal().fences(terminal)))
            .map(|session| session.terminal().clone())
            .collect::<Vec<_>>();
        for terminal in stale {
            self.close_terminal(&terminal);
        }
        if let Some(terminal) = focused
            && !self
                .terminals
                .iter()
                .any(|session| session.terminal().fences(terminal))
        {
            self.start_terminal_session(terminal.clone(), geometry);
        }
    }

    /// Ask the daemon for the runtimes still live in this workspace's scopes.
    /// A missing port (embedder) or a launch worker that has temporarily taken
    /// it yields an empty inventory rather than an error, so restore simply
    /// finds nothing. A daemon failure is surfaced so the caller restores
    /// nothing instead of guessing.
    #[cfg(test)]
    fn list_open_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, ()> {
        match self
            .agent
            .as_mut()
            .and_then(|agent| agent.port.as_deref_mut())
        {
            Some(port) => port.list_terminals().map_err(|_| ()),
            None => Ok(Vec::new()),
        }
    }

    fn resize_terminals(&mut self, geometry: Geometry) {
        let Some(port) = self
            .agent
            .as_mut()
            .and_then(|agent| agent.port.as_deref_mut())
        else {
            return;
        };
        for session in &mut self.terminals {
            session.resize(&mut AgentStreamPort(port), geometry);
        }
    }

    /// Forward raw passthrough bytes to the live terminal `terminal`. Returns
    /// an error when the port is busy or the matching session cannot accept it.
    fn send_terminal_bytes(&mut self, terminal: &TerminalRef, bytes: &[u8]) -> Result<(), String> {
        let Some(port) = self
            .agent
            .as_mut()
            .and_then(|agent| agent.port.as_deref_mut())
        else {
            return Err("terminal is busy; keystroke not delivered".to_owned());
        };
        let Some(session) = self
            .terminals
            .iter_mut()
            .find(|session| session.terminal().fences(terminal))
        else {
            return Err("terminal session is no longer available".to_owned());
        };
        match session.send_input(&mut AgentStreamPort(port), bytes) {
            Ok(()) => Ok(()),
            Err(error) => Err(error.message()),
        }
    }

    /// Poll every attached terminal once and return the refs of those the daemon
    /// reports as exited. Polling all of them (not just the focused pane) is what
    /// lets a background tab whose shell ran `exit` be detected and closed.
    fn poll_all_terminals(&mut self) -> Vec<TerminalRef> {
        let Some(agent) = self.agent.as_mut() else {
            return Vec::new();
        };
        let Some(port) = agent.port.as_deref_mut() else {
            return Vec::new();
        };
        let mut reconnected = false;
        let exited = self
            .terminals
            .iter_mut()
            .filter_map(|session| {
                let before = session.state();
                session.poll(&mut AgentStreamPort(port));
                if before == SessionState::Reconnecting && session.state() == SessionState::Live {
                    reconnected = true;
                }
                (session.state() == SessionState::Exited).then(|| session.terminal().clone())
            })
            .collect();
        self.terminal_reconnected |= reconnected;
        exited
    }

    fn take_terminal_reconnected(&mut self) -> bool {
        std::mem::take(&mut self.terminal_reconnected)
    }

    /// Release a terminal's client subscription and drop its coordinator. The
    /// daemon keeps the process; only this TUI detaches. Safe when no session
    /// matches (already pruned).
    fn close_terminal(&mut self, terminal: &TerminalRef) {
        if let Some(port) = self
            .agent
            .as_mut()
            .and_then(|agent| agent.port.as_deref_mut())
            && let Some(session) = self
                .terminals
                .iter_mut()
                .find(|session| session.terminal().fences(terminal))
        {
            session.detach(&mut AgentStreamPort(port));
        }
        self.terminals
            .retain(|session| !session.terminal().fences(terminal));
    }

    fn agent_continuation_for(&self, terminal: &TerminalRef) -> Option<AgentContinuationRef> {
        self.agent_tab_intent.as_ref().and_then(|context| {
            context
                .state
                .targets
                .iter()
                .find_map(|target| {
                    target
                        .tabs
                        .iter()
                        .find(|slot| slot.terminal.fences(terminal))
                        .map(|slot| slot.continuation)
                })
                .or_else(|| {
                    context
                        .visible_agents
                        .iter()
                        .find(|(visible, _)| visible.fences(terminal))
                        .map(|(_, continuation)| *continuation)
                })
        })
    }

    fn observe_agent_tabs(
        &mut self,
        terminals: Vec<TerminalInventoryEntry>,
        agents: AgentInventory,
    ) -> Result<AgentTabObservation, AgentTabIntentError> {
        let Some(context) = self.agent_tab_intent.as_mut() else {
            return Ok(AgentTabObservation {
                projection: AgentTabProjection::default(),
                cas_accepted: true,
            });
        };
        let commit = context.port.mutate(
            context.workspace,
            context.state.revision,
            AgentTabIntentMutation::Observe {
                terminals,
                agents,
                allowed_sessions: context.allowed_sessions.clone(),
            },
        )?;
        context.state = commit.intent;
        let projection = commit.projection.unwrap_or_default();
        if commit.mutation_applied {
            context.visible_agents = projection
                .targets
                .iter()
                .flat_map(|target| &target.tabs)
                .map(|slot| (slot.terminal.clone(), slot.continuation))
                .collect();
        }
        Ok(AgentTabObservation {
            projection,
            cas_accepted: commit.mutation_applied,
        })
    }

    fn mutate_agent_intent(
        &mut self,
        mutation: AgentTabIntentMutation,
    ) -> Result<(), AgentTabIntentError> {
        let Some(context) = self.agent_tab_intent.as_mut() else {
            return Ok(());
        };
        let commit = context
            .port
            .mutate(context.workspace, context.state.revision, mutation)?;
        context.state = commit.intent;
        if !commit.mutation_applied {
            return Err(AgentTabIntentError::ConcurrentChange);
        }
        Ok(())
    }

    fn request_agent_observation(&mut self) {
        self.agent_observation_requested = true;
    }

    fn take_agent_observation_request(&mut self) -> bool {
        std::mem::take(&mut self.agent_observation_requested)
    }

    fn agent_reopen_choices(&self) -> Vec<AgentReopenChoice> {
        self.agent_tab_intent
            .as_ref()
            .map_or_else(Vec::new, |context| {
                context
                    .state
                    .dismissed
                    .iter()
                    .map(|continuation| AgentReopenChoice {
                        label: AgentTabIntent::safe_label(*continuation),
                        continuation: *continuation,
                    })
                    .collect()
            })
    }

    fn persist_agent_order(
        &mut self,
        session_id: Option<SessionId>,
        current_terminals: &[TerminalRef],
        next_terminals: &[TerminalRef],
    ) -> Result<(), AgentTabIntentError> {
        let current = current_terminals
            .iter()
            .filter_map(|terminal| self.agent_continuation_for(terminal))
            .collect::<Vec<_>>();
        let continuations = next_terminals
            .iter()
            .filter_map(|terminal| self.agent_continuation_for(terminal))
            .collect::<Vec<_>>();
        if current == continuations {
            return Ok(());
        }
        self.mutate_agent_intent(AgentTabIntentMutation::Reorder {
            session_id,
            continuations,
        })
    }

    fn has_agent_intent_for(&self, session_id: Option<SessionId>) -> bool {
        self.agent_tab_intent.as_ref().is_some_and(|context| {
            context
                .state
                .targets
                .iter()
                .find(|target| target.session_id == session_id)
                .is_some_and(|target| !target.tabs.is_empty())
        })
    }

    fn set_allowed_agent_sessions(&mut self, sessions: impl IntoIterator<Item = SessionId>) {
        let sessions = sessions.into_iter().collect::<BTreeSet<_>>();
        let changed = self
            .agent_tab_intent
            .as_ref()
            .is_some_and(|context| context.allowed_sessions != sessions);
        if let Some(context) = self.agent_tab_intent.as_mut() {
            context.allowed_sessions = sessions;
        }
        if changed {
            // Lifecycle membership is authoritative for target retention. Use
            // the same coalesced controller request as Reopen so an idle,
            // already-successful controller observes removals exactly once;
            // an in-flight job is fenced and an outage keeps its backoff.
            self.request_agent_observation();
        }
    }

    /// Project the already-polled rows for `terminal`, optionally highlighting an
    /// in-progress selection. Returns `None` when no attached session matches.
    fn terminal_rows(
        &self,
        terminal: &TerminalRef,
        selection: Option<&TerminalSelection>,
    ) -> Option<Vec<String>> {
        let session = self
            .terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))?;
        Some(match selection {
            Some(selection) => session.display_rows_with_scrollback_selection(selection),
            None => session.display_rows_with_scrollback(),
        })
    }

    /// The stable visible cells for `terminal`, snapshotted so a drag selection
    /// stays fixed while later output arrives. `None` when no session matches.
    fn terminal_cells(&self, terminal: &TerminalRef) -> Option<Vec<String>> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(TerminalSession::cells)
    }

    fn terminal_error(&self, terminal: &TerminalRef) -> Option<&str> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .and_then(TerminalSession::error)
    }
}

/// welcome のメニュー操作を画面遷移へ写す。
fn welcome_action(action: MenuAction) -> WelcomeStep {
    match action {
        MenuAction::Quit => WelcomeStep::Quit,
        MenuAction::Open => WelcomeStep::OpenList,
        MenuAction::OpenRecent(index) => WelcomeStep::OpenRecent(index),
        MenuAction::New => WelcomeStep::NewForm,
        MenuAction::Config => WelcomeStep::ConfigScreen,
    }
}

/// Config 画面のキー処理。Save は dirty な Save 行でのみ有効で、Enter は save フローを
/// 開始（loading）する。保存中の再入力は `begin_save` が弾く。
#[allow(clippy::needless_pass_by_value)]
fn step_config(config: &mut Config, key: Key, _settings: &mut dyn SettingsPort) -> ConfigStep {
    match key {
        Key::Up | Key::Char('k') => {
            config.previous_field();
            ConfigStep::Stay
        }
        Key::Down | Key::Char('j') => {
            config.next_field();
            ConfigStep::Stay
        }
        Key::Left | Key::Char('h') => {
            config.cycle_selected(false);
            ConfigStep::Stay
        }
        Key::Right | Key::Char('l') => {
            config.cycle_selected(true);
            ConfigStep::Stay
        }
        // Enter begins the save flow (loading). `begin_save` is a no-op unless a
        // dirty Save row is focused with no save already in flight, so a rapid
        // second Enter cannot start a second save.
        Key::Enter if config.begin_save() => ConfigStep::Save,
        Key::Escape => ConfigStep::Back,
        Key::Quit | Key::CtrlQ => ConfigStep::Quit,
        _ => ConfigStep::Stay,
    }
}

/// Workspace Config is an overlay owned by Home, so global quit chords must not
/// escape to the enclosing workspace loop while it has input focus. The full
/// screen Config keeps its existing quit contract through [`step_config`].
fn step_workspace_config(
    config: &mut Config,
    key: Key,
    settings: &mut dyn SettingsPort,
) -> WorkspaceConfigStep {
    match step_config(config, key, settings) {
        ConfigStep::Stay | ConfigStep::Quit => WorkspaceConfigStep::Stay,
        ConfigStep::Back => WorkspaceConfigStep::Back,
        ConfigStep::Save => WorkspaceConfigStep::Save,
    }
}

/// Run Config from an opened workspace. The form contains only workspace-owned
/// settings and returns to the still-live Home runtime after Escape or save.
fn run_workspace_config(
    term: &mut dyn Terminal,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
    base: &[String],
) -> io::Result<()> {
    let mut form = Config::load_workspace_with_available_models(settings, available_models);
    loop {
        let (height, width) = term.size()?;
        term.draw(&config::render_over(height, width, base, &form))?;
        match step_workspace_config(&mut form, term.read_key()?, settings) {
            WorkspaceConfigStep::Stay => {}
            WorkspaceConfigStep::Back => return Ok(()),
            WorkspaceConfigStep::Save => {
                play_config_save_wave(term, &mut form, Some(base))?;
                if form.commit_save(settings) {
                    let (height, width) = term.size()?;
                    term.draw(&config::render_over(height, width, base, &form))?;
                    term.wait(config::DONE_DISPLAY)?;
                    form.reset_save();
                    return Ok(());
                }
            }
        }
    }
}

/// welcome 画面のキー処理。最上位画面なので Esc も終了として扱う。
#[allow(clippy::needless_pass_by_value)]
fn step_welcome(welcome: &mut Welcome, key: Key) -> WelcomeStep {
    match key {
        Key::Up | Key::Char('k') => {
            welcome.select_prev();
            WelcomeStep::Stay
        }
        Key::Down | Key::Char('j') => {
            welcome.select_next();
            WelcomeStep::Stay
        }
        Key::Escape | Key::Quit | Key::CtrlQ => WelcomeStep::Quit,
        Key::Enter => welcome_action(welcome.selected_action()),
        Key::Char(ch) => welcome
            .action_for(ch)
            .map_or(WelcomeStep::Stay, welcome_action),
        Key::Left
        | Key::Right
        | Key::Home
        | Key::End
        | Key::Delete
        | Key::LineStart
        | Key::LineEnd
        | Key::SelectLeft
        | Key::SelectRight
        | Key::SelectHome
        | Key::SelectEnd
        | Key::Backspace
        | Key::Tab
        | Key::CtrlD
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Paste(_)
        | Key::TerminalCopy { .. }
        | Key::Other => WelcomeStep::Stay,
    }
}

/// New 画面のキー処理（純粋）。矢印キーでフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
#[allow(clippy::needless_pass_by_value)]
fn step_new(form: &mut New, key: Key) -> NewStep {
    match key {
        Key::Up => {
            form.focus_prev();
            NewStep::Stay
        }
        Key::Down => {
            form.focus_next();
            NewStep::Stay
        }
        Key::Left => {
            step_new_horizontal(form, false);
            NewStep::Stay
        }
        Key::Right => {
            step_new_horizontal(form, true);
            NewStep::Stay
        }
        // Home/End と emacs 行頭/行末（Ctrl-A/Ctrl-E）はフォーカス中フィールドの
        // キャレット移動。テキスト入力にフォーカスがあるので new-session ではなく caret。
        Key::Home | Key::LineStart => {
            form.cursor_home();
            NewStep::Stay
        }
        Key::End | Key::LineEnd => {
            form.cursor_end();
            NewStep::Stay
        }
        Key::SelectLeft => {
            form.select_left();
            NewStep::Stay
        }
        Key::SelectRight => {
            form.select_right();
            NewStep::Stay
        }
        Key::SelectHome => {
            form.select_home();
            NewStep::Stay
        }
        Key::SelectEnd => {
            form.select_end();
            NewStep::Stay
        }
        Key::Backspace => {
            form.backspace();
            NewStep::Stay
        }
        Key::Delete => {
            form.delete_forward();
            NewStep::Stay
        }
        Key::Char(ch) => {
            form.insert_char(ch);
            NewStep::Stay
        }
        // A bracketed paste inserts its text into the focused field verbatim, so
        // a repository URL or path pastes as one block.
        Key::Paste(text) => {
            for ch in text.chars() {
                form.insert_char(ch);
            }
            NewStep::Stay
        }
        Key::Escape => NewStep::Back,
        Key::Quit | Key::CtrlQ => NewStep::Quit,
        Key::Tab => {
            form.complete_directory();
            NewStep::Stay
        }
        // Enter は入力を検証して作成へ進む。必須項目が欠けていれば安全なメッセージを
        // notice に出し、同画面に留まって draft を保つ。
        Key::Enter => match form.to_request() {
            Ok(request) => NewStep::Create(request),
            Err(error) => {
                form.set_notice(Some(error.message().to_owned()));
                NewStep::Stay
            }
        },
        Key::CtrlD
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::TerminalCopy { .. }
        | Key::Other => NewStep::Stay,
    }
}

/// 作成失敗の io error を、New フォームの 1 行 notice slot に収まる安全なメッセージへ縮める。
/// git の stderr は複数行になりうるので先頭行だけを取り、長すぎる場合は切り詰める。
fn new_project_notice(error: &io::Error) -> String {
    const MAX: usize = 72;
    let message = error.to_string();
    let first = message.lines().next().unwrap_or("").trim();
    let detail = if first.is_empty() {
        "could not create the project"
    } else {
        first
    };
    if detail.chars().count() > MAX {
        let truncated: String = detail.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        detail.to_owned()
    }
}

/// New 画面の ←→ 操作。モード選択にフォーカスがあるときはモードを切り替え、テキスト欄では
/// キャレットを左右へ動かす（`right` が右方向）。
fn step_new_horizontal(form: &mut New, right: bool) {
    if form.focus() == Field::Mode {
        form.toggle_mode();
    } else if right {
        form.cursor_right();
    } else {
        form.cursor_left();
    }
}

/// Open 画面のキー処理。Enter で選択 path を確定し、Esc で welcome へ戻る。
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn step_open(open: &mut Open, key: Key) -> OpenStep {
    if open.unregistering_path().is_some() {
        return match key {
            Key::Left | Key::Right | Key::Tab => {
                open.toggle_unregister_choice();
                OpenStep::Stay
            }
            Key::Char('y' | 'Y') | Key::Enter => open
                .confirm_unregister()
                .map_or(OpenStep::Stay, OpenStep::ConfirmUnregister),
            Key::Char('n' | 'N') | Key::Escape => {
                open.cancel_unregister();
                OpenStep::Stay
            }
            Key::Quit | Key::CtrlQ => OpenStep::Quit,
            _ => OpenStep::Stay,
        };
    }
    if open.cleanup_confirming() {
        return match key {
            Key::Char('y') | Key::Enter => OpenStep::ConfirmCleanup,
            Key::Char('n') | Key::Escape => {
                open.cancel_cleanup();
                OpenStep::Stay
            }
            Key::Quit | Key::CtrlQ => OpenStep::Quit,
            _ => OpenStep::Stay,
        };
    }
    match key {
        Key::Up => {
            open.select_prev();
            OpenStep::Stay
        }
        Key::Down => {
            open.select_next();
            OpenStep::Stay
        }
        Key::Backspace => {
            open.pop_filter();
            OpenStep::Stay
        }
        Key::Left => {
            open.filter_left();
            OpenStep::Stay
        }
        Key::Right => {
            open.filter_right();
            OpenStep::Stay
        }
        Key::Home | Key::LineStart => {
            open.filter_home();
            OpenStep::Stay
        }
        Key::End | Key::LineEnd => {
            open.filter_end();
            OpenStep::Stay
        }
        Key::Delete => {
            open.filter_delete_forward();
            OpenStep::Stay
        }
        Key::SelectLeft => {
            open.filter_select_left();
            OpenStep::Stay
        }
        Key::SelectRight => {
            open.filter_select_right();
            OpenStep::Stay
        }
        Key::SelectHome => {
            open.filter_select_home();
            OpenStep::Stay
        }
        Key::SelectEnd => {
            open.filter_select_end();
            OpenStep::Stay
        }
        Key::Escape => OpenStep::Back,
        Key::Quit | Key::CtrlQ => OpenStep::Quit,
        Key::Enter => {
            let paths = if open.is_unite() {
                open.unite_paths()
            } else {
                open.selected()
                    .map(|workspace| vec![workspace.path.clone()])
                    .unwrap_or_default()
            };
            paths
                .into_iter()
                .next()
                .map_or(OpenStep::Stay, OpenStep::Choose)
        }
        Key::Tab => {
            open.toggle_unite();
            OpenStep::Stay
        }
        Key::Char(' ') if open.is_unite() => {
            open.toggle_unite_member();
            OpenStep::Stay
        }
        Key::Char('C') => {
            open.request_cleanup();
            OpenStep::Stay
        }
        Key::CtrlD => {
            open.request_unregister();
            OpenStep::Stay
        }
        Key::Char(ch) => {
            open.push_filter(ch);
            OpenStep::Stay
        }
        // A bracketed paste appends its text to the filter one character at a time.
        Key::Paste(text) => {
            for ch in text.chars() {
                open.push_filter(ch);
            }
            OpenStep::Stay
        }
        Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::TerminalCopy { .. }
        | Key::Other => OpenStep::Stay,
    }
}

/// Run one daemon-owned session command without blocking the terminal event
/// loop. Admission is bounded to one worker; a concurrent request completes as
/// Busy without reaching the shared daemon port.
fn begin_session_command(
    ui: &mut WorkspaceUi,
    command: SessionCommand,
    completion: SessionBackendCompletion,
) -> bool {
    if ui.active_session_command.is_some() {
        emit_session_command_result(
            &Err("session command is already running".to_owned()),
            &completion,
        );
        return false;
    }
    let command_id = ui.next_session_command;
    ui.next_session_command = ui.next_session_command.wrapping_add(1);
    ui.active_session_command = Some(command_id);
    let port = std::sync::Arc::clone(&ui.session_commands);
    let workspace = ui.workspace.record().clone();
    let sender = ui.session_completion_sender.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            port.execute(&workspace, None, command)
        }))
        .unwrap_or_else(|_| Err("session command worker failed".to_owned()));
        // Complete the reducer request before returning the projection/port to
        // the UI. If the workspace exited, the sink is closed harmlessly but
        // the accepted Effect still took exactly one completion path.
        emit_session_command_result(&result, &completion);
        let _ = sender.send(SessionCommandCompletion {
            command_id,
            result,
            completion,
        });
    });
    true
}

/// A terminal wake-up is a bounded opportunity to adopt lifecycle changes made
/// by another client, such as an MCP server. Never enqueue a refresh while the
/// a lifecycle command is already in flight: its revision-based completion
/// reconciliation handles the newer observation safely.
fn tick_session_refresh(
    key: &Key,
    session_command_available: bool,
    workspace: WorkspaceId,
) -> Option<Effect> {
    (matches!(key, Key::Other) && session_command_available)
        .then_some(Effect::RefreshSessions { workspace })
}

/// The daemon-owned name for the session identified by `session`, if the current
/// sidebar projection still holds it. A `RemoveSession` effect carries the stable
/// identity, while the session command port speaks the daemon-facing name.
fn session_name_for(ui: &WorkspaceUi, session: SessionId) -> Option<String> {
    ui.workspace
        .session_ids()
        .iter()
        .zip(ui.workspace.sessions())
        .find_map(|(id, record)| (*id == session).then(|| record.name.clone()))
}

/// Reconcile sidebar rows and the IDs used by Agent/terminal requests as one
/// daemon-authoritative observation.  Legacy/test ports may provide rows only;
/// they retain the existing non-runtime projection behaviour.
fn apply_session_projection(
    ui: &mut WorkspaceUi,
    sessions: Option<Vec<usagi_core::domain::session::SessionRecord>>,
    session_ids: Option<Vec<SessionId>>,
    agent_resumes: Option<BTreeMap<SessionId, ProviderResumeProjection>>,
) {
    let Some(sessions) = sessions else {
        return;
    };
    if let Some(session_ids) = session_ids.filter(|ids| ids.len() == sessions.len()) {
        ui.workspace
            .replace_sessions_with_runtime_ids(sessions, session_ids.clone());
        if let Some(agent) = ui.agent.as_mut() {
            agent.sessions = session_ids;
        }
    } else {
        ui.workspace.replace_sessions(sessions);
    }
    if let Some(agent_resumes) = agent_resumes {
        ui.agent_resumes = agent_resumes;
    }
}

/// Receive completed create/remove workers before drawing the next frame. The
/// returned port is reclaimed for the next command and a successful daemon
/// snapshot is reconciled into the session cache, which [`sync_runtime_sessions`]
/// then promotes into the controller's Home rows. A failure is no longer dropped
/// silently: the port's message is display-safe by contract and is collapsed to a
/// safe single line before it reaches the screen. A create failure refluxes as a
/// failed [`OperationResult`] so its pending row clears and the safe message opens
/// the create-failure dialog; any other failure (e.g. remove) refluxes as a
/// controller [`BackendEvent::Notice`]. Both are distinct from an in-form local
/// validation error.
fn drain_session_completions(ui: &mut WorkspaceUi) {
    while let Ok(completion) = ui.session_completions.try_recv() {
        if ui.active_session_command != Some(completion.command_id) {
            continue;
        }
        ui.active_session_command = None;
        match &completion.completion {
            SessionBackendCompletion::Create { .. } => ui.creating_session = None,
            SessionBackendCompletion::Remove { session, .. }
                if ui.removing_session == Some(*session) =>
            {
                ui.removing_session = None;
            }
            SessionBackendCompletion::Refresh { .. } | SessionBackendCompletion::Remove { .. } => {}
        }
        if let Ok(result) = completion.result {
            let is_current = result
                .revision
                .is_none_or(|revision| revision >= ui.last_session_revision);
            if let Some(revision) = result.revision.filter(|_| is_current) {
                ui.last_session_revision = revision;
            }
            if is_current {
                apply_session_projection(
                    ui,
                    result.sessions,
                    result.session_ids,
                    result.agent_resumes,
                );
            }
        }
    }
}

/// Emit the exactly-one reducer completion owned by one admitted command.
/// Projection and port recovery are deliberately separate so workspace exit or
/// a closed host channel cannot strand controller pending state.
fn emit_session_command_result(
    result: &Result<SessionCommandResult, String>,
    completion: &SessionBackendCompletion,
) {
    match (result, completion) {
        (
            Ok(result),
            SessionBackendCompletion::Create {
                token,
                before,
                completions,
            },
        ) => {
            let created = result
                .session_ids
                .as_ref()
                .and_then(|ids| ids.iter().copied().find(|id| !before.contains(id)));
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: *token,
                succeeded: created.is_some(),
                created,
                notice: Some(Notice::new(if created.is_some() {
                    "session created"
                } else {
                    "daemon did not return the created session"
                })),
            }));
        }
        (
            Ok(result),
            SessionBackendCompletion::Refresh {
                before,
                completions,
            }
            | SessionBackendCompletion::Remove {
                before,
                completions,
                ..
            },
        ) => {
            completions.emit(AppEvent::Backend(BackendEvent::Sessions(
                result.session_ids.clone().unwrap_or_else(|| before.clone()),
            )));
        }
        (
            Err(message),
            SessionBackendCompletion::Create {
                token, completions, ..
            },
        ) => {
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: *token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new(safe_session_error(message))),
            }));
        }
        (
            Err(message),
            SessionBackendCompletion::Refresh { completions, .. }
            | SessionBackendCompletion::Remove { completions, .. },
        ) => {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                safe_session_error(message),
            ))));
        }
    }
}

/// Collapse a daemon session-command error into a safe single line for the
/// create-failure dialog: take the first line only, so multi-line stderr or
/// internal detail on later lines never leaks onto the screen. The line is kept
/// in full — the dialog wraps it to the box width and shows all of it, so no
/// length cap truncates a legitimate error into an ellipsis.
fn safe_session_error(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        "could not create the session".to_owned()
    } else {
        first.to_owned()
    }
}

/// Start one daemon launch after its pending tab has reached the terminal.
///
/// The port travels with the worker and comes back through
/// [`PaneLaunchCompletion`]. This is deliberately the same ownership pattern
/// as session creation: a slow daemon request never blocks input, wave redraws,
/// or the interaction marker that suppresses automatic focus.
fn drain_pane_launches(ui: &mut WorkspaceUi, geometry: Geometry) {
    let mut launches = std::mem::take(&mut ui.pane_launches);
    while !launches.is_empty() {
        let launch = launches.remove(0);
        match launch {
            PaneLaunch::Agent {
                operation,
                workspace,
                session,
                profile,
                resume,
            } => {
                let Some(mut port) = ui.agent.as_mut().and_then(|agent| agent.port.take()) else {
                    ui.pane_launches.push(PaneLaunch::Agent {
                        operation,
                        workspace,
                        session,
                        profile,
                        resume,
                    });
                    continue;
                };
                let sender = ui.pane_completion_sender.clone();
                std::thread::spawn(move || {
                    let result = if resume {
                        session.map_or_else(
                            || Err("workspace-root Agent resume is unavailable".to_owned()),
                            |session| port.resume(workspace, session, operation),
                        )
                    } else {
                        port.launch(workspace, session, profile)
                    };
                    let _ = sender.send(PaneLaunchCompletion {
                        port,
                        outcome: PaneLaunchOutcome::Agent { operation, result },
                    });
                });
                // Only one worker may own this stateful daemon port. Remaining
                // requests stay visibly pending and start after completion.
                ui.pane_launches.append(&mut launches);
                return;
            }
            PaneLaunch::Terminal {
                operation,
                workspace,
                session,
                arguments,
            } => {
                let Some(mut port) = ui.agent.as_mut().and_then(|agent| agent.port.take()) else {
                    ui.pane_launches.push(PaneLaunch::Terminal {
                        operation,
                        workspace,
                        session,
                        arguments,
                    });
                    continue;
                };
                let sender = ui.pane_completion_sender.clone();
                std::thread::spawn(move || {
                    let result = port.launch_terminal(workspace, session, geometry, &arguments);
                    let _ = sender.send(PaneLaunchCompletion {
                        port,
                        outcome: PaneLaunchOutcome::Terminal { operation, result },
                    });
                });
                ui.pane_launches.append(&mut launches);
                return;
            }
        }
    }
}

/// Translates a presentation [`Key`] into the controller's [`AppEvent`] vocabulary
/// for the real-terminal runtime that routes Home input through `update()`.
///
/// The composition-root adapter has already resolved the `Ctrl-O` live prefix, so
/// [`Key::Live`] arrives as a settled [`LiveTerminalAction`] that this function
/// maps to the equivalent [`AppKey`]. Ordinary keys map one-to-one; the reducer,
/// which owns overlay context, decides what each means. `Key::Other` (resize and
/// backend wakeups the composition root cannot express as input) advances the
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
        Key::Live(action) => return live_action_to_app_key(action).map(AppEvent::Key),
        Key::Other => return Some(AppEvent::Tick),
        Key::Up => AppKey::Up,
        Key::Down => AppKey::Down,
        // Left/Right move the focus inside a horizontal choice (the Yes/No quit
        // confirmation); the reducer ignores them elsewhere. Tab motion between
        // live tabs stays Ctrl-N/P.
        Key::Left => AppKey::Left,
        Key::Right => AppKey::Right,
        Key::Enter => AppKey::Enter,
        Key::Backspace => AppKey::Backspace,
        Key::Tab => AppKey::Tab,
        Key::Escape => AppKey::Escape,
        // Runtime adapters preserve Ctrl-A as U+0001. `Ctrl-A` (LineStart) and
        // `Home` both mean `+ new session` here, where no text field owns focus:
        // the sidebar-navigation contract from #257/#287 that this issue keeps
        // intact. A focused palette / create form intercepts these before the
        // reducer, so caret motion never reaches this navigation branch.
        Key::LineStart | Key::Home | Key::Char('\u{1}') => AppKey::CtrlA,
        Key::Char(character) => AppKey::Char(character),
        Key::Quit => AppKey::CtrlC,
        Key::CtrlQ => AppKey::CtrlQ,
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
        // pointer drags and clicks (a shell + `TerminalSession` concern), Ctrl-D
        // (Open Workspace only), and the caret/selection keys that have meaning
        // only inside a focused text field (End/Ctrl-E, Delete, Shift+arrows).
        Key::Passthrough(_)
        // Home navigation and its overlay text fields (`:` palette, create-session
        // name, tab rename) do not consume a bracketed paste; the live pane owns
        // paste via `key_to_terminal_bytes`.
        | Key::Paste(_)
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

/// Maps a resolved live-terminal action to its Home reducer key. Tab close and
/// terminal scroll/copy stay pane- and shell-level concerns the Home reducer has
/// no vocabulary for, so they return `None`.
fn live_action_to_app_key(action: LiveTerminalAction) -> Option<AppKey> {
    match action {
        LiveTerminalAction::Switch => Some(AppKey::CtrlO),
        LiveTerminalAction::OpenCloseupModal => Some(AppKey::OpenCloseupOverlay),
        LiveTerminalAction::NextTab => Some(AppKey::CtrlN),
        LiveTerminalAction::PreviousTab => Some(AppKey::CtrlP),
        LiveTerminalAction::Agent => Some(AppKey::CtrlA),
        LiveTerminalAction::QuitConfirmation => Some(AppKey::OpenQuitConfirmation),
        LiveTerminalAction::CloseTab
        | LiveTerminalAction::MoveTabNext
        | LiveTerminalAction::MoveTabPrevious
        | LiveTerminalAction::ScrollUp
        | LiveTerminalAction::ScrollDown => None,
    }
}

fn terminal_geometry(height: usize, width: usize) -> Geometry {
    let (rows, cols) = workspace::terminal_viewport(height, width);
    Geometry {
        cols: u16::try_from(cols.min(usize::from(u16::MAX)))
            .expect("clamped terminal width fits u16"),
        rows: u16::try_from(rows.min(usize::from(u16::MAX)))
            .expect("clamped terminal height fits u16"),
    }
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

/// Recent が指す単体 workspace path。Unite の runtime は今回の対象外なので開かない。
fn recent_path(recent: &Recent) -> Option<&Path> {
    match recent {
        Recent::Workspace(overview) => Some(&overview.workspace.path),
        Recent::Unite(_) => None,
    }
}

/// Project the daemon-authoritative session records into the controller's Home
/// row material, in the same order the runtime holds their IDs.
fn project_controller_sessions(ui: &WorkspaceUi) -> Vec<ProjectedSession> {
    ui.workspace
        .sessions()
        .iter()
        .zip(ui.workspace.session_ids())
        .map(|(record, id)| {
            let mut projected = ProjectedSession::from_record(*id, record);
            projected.removing = ui.removing_session == Some(*id);
            projected.agent_resume = ui.agent_resumes.get(id).copied();
            projected
        })
        .collect()
}

/// Render a single static Home frame from a workspace snapshot, using the same
/// controller projection as the interactive loop.
///
/// This is the non-interactive `usagi launch <path>` fallback (no terminal), so
/// it shows the initial Home surface: root selected/active, the snapshot's
/// sessions, and the `+ new session` row.
#[must_use]
pub fn render_home_snapshot(
    height: usize,
    width: usize,
    snapshot: &WorkspaceSnapshot,
) -> Vec<String> {
    let workspace = WorkspaceView::with_runtime_ids(
        snapshot.workspace.clone(),
        snapshot.state.clone(),
        snapshot.session_ids.clone(),
    );
    let sessions: Vec<ProjectedSession> = workspace
        .sessions()
        .iter()
        .zip(workspace.session_ids())
        .map(|(record, id)| ProjectedSession::from_record(*id, record))
        .collect();
    let state = AppState::home(snapshot.workspace_id, snapshot.session_ids.clone());
    let projection = HomeProjection::from_state(
        &state,
        &snapshot.workspace.name,
        &snapshot.workspace.path,
        &sessions,
    );
    render_home(height, width, &projection)
}

/// Keep the controller's Home rows in step with the daemon session projection
/// the legacy transport reconciled this frame.
fn sync_runtime_sessions(runtime: &mut WorkspaceRuntime, ui: &WorkspaceUi) {
    let ids = ui.workspace.session_ids().to_vec();
    if runtime.state().sessions() != ids.as_slice() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Sessions(ids)));
    }
    // Keep the reducer's advisory name copy in step so the create form can reject
    // a known worktree collision locally before it ever reaches the daemon. The
    // lifecycle snapshot supplies managed sessions; the directory scan also
    // catches a stale `.usagi/sessions/<name>` that has no lifecycle record.
    let mut names: std::collections::BTreeSet<String> = ui
        .workspace
        .sessions()
        .iter()
        .map(|record| record.name.clone())
        .collect();
    names.extend(session_worktree_names(ui.workspace.path()));
    let names: Vec<String> = names.into_iter().collect();
    if runtime.state().session_names() != names.as_slice() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionNames(names)));
    }
}

/// Names of worktree directories which would collide with a new session.
///
/// This is a read-only, best-effort preflight fact for the inline form. The
/// daemon remains the sole authority that creates or removes worktrees; an
/// unreadable directory simply contributes no local hint and is checked again
/// by the daemon when the user submits the request.
fn session_worktree_names(workspace: &Path) -> Vec<String> {
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

/// Project the focused live terminal's already-polled rows for
/// `with_terminal_view`, folding in the shell-owned scroll offset, selection
/// highlight, and copy feedback tracked by `controls`. Focus changes reset those
/// controls so nothing leaks between panes.
fn controller_terminal_view(
    ui: &WorkspaceUi,
    runtime: &WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    viewport_rows: usize,
) -> Option<TerminalViewProjection> {
    let terminal = runtime.focused_terminal();
    controls.sync_focus(terminal.as_ref());
    let terminal = terminal?;
    let rows = ui.terminal_rows(&terminal, controls.selection())?;
    let mut projection = controls.project(rows, viewport_rows);
    if let Some(error) = ui.terminal_error(&terminal) {
        projection.feedback = Some(error.to_owned());
    }
    Some(projection)
}

/// Run the per-frame foreground-terminal sweep: poll the one attached selection,
/// auto-close it if exited, then project its freshly polled viewport. Returns
/// the projection plus its `(rows_len, scroll)` so a later pointer drag maps back
/// to the exact retained cell.
fn poll_and_project_terminals(
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    geometry: Geometry,
) -> (Option<TerminalViewProjection>, usize, usize) {
    close_exited_panes(ui, runtime);
    let terminal_view = controller_terminal_view(ui, runtime, controls, usize::from(geometry.rows));
    let (rows_len, scroll) = match &terminal_view {
        Some(view) => (view.rows.len(), view.scroll),
        None => (0, 0),
    };
    (terminal_view, rows_len, scroll)
}

/// Poll the attached foreground terminal and auto-close it if the daemon reports exit:
/// the runtime drops the tab (clearing `has_live_pane` when it was the last) and
/// the shell detaches the client subscription. This restores the pre-migration
/// `close_exited_terminal` sweep so an `exit` in a live shell no longer strands a
/// Live tab.
fn close_exited_panes(ui: &mut WorkspaceUi, runtime: &mut WorkspaceRuntime) {
    for terminal in ui.poll_all_terminals() {
        let _ = runtime.exit_pane(shell_target_for_terminal(&terminal), terminal.clone());
        ui.close_terminal(&terminal);
    }
}

/// The pane target a terminal ref belongs to. Mirrors the pane reducer's own
/// mapping so the shell routes an exit to the same registry entry.
fn shell_target_for_terminal(terminal: &TerminalRef) -> Target {
    terminal
        .session_id
        .map_or(Target::Root(terminal.workspace_id), Target::Session)
}

/// Run restore over a dedicated daemon port. Inventory is retried with bounded
/// backoff on this worker, so the first frame and terminal input loop never wait
/// for a handshake or a slow daemon response.
fn spawn_restore_job(
    mut port: Box<dyn AgentCommandPort>,
    workspace: WorkspaceId,
    allowed_sessions: BTreeSet<SessionId>,
    dispatched_interaction: u64,
    dispatched_registry_revision: u64,
    sender: Sender<RestoreCompletion>,
) {
    std::thread::spawn(move || {
        let mut terminals = Err(TerminalError::Unavailable);
        let mut agents = Err("Agent inventory is unavailable".to_owned());
        let mut observation_coherent = false;
        for attempt in 0..3 {
            // Bracket the Agent inventory with terminal snapshots. Equal
            // canonical snapshots plus a bijective live-Agent relationship are
            // the optimistic consistency fence available without expanding the
            // IPC protocol in #506.
            let before = port.list_terminals();
            let agent_attempt = port.resume_inventory(workspace).and_then(|inventory| {
                if inventory.workspace_id == workspace {
                    Ok(inventory)
                } else {
                    Err("Agent inventory scope changed while restoring".to_owned())
                }
            });
            let after = port.list_terminals();
            match (before, agent_attempt, after) {
                (Ok(mut before), Ok(inventory), Ok(mut after)) => {
                    normalize_terminal_inventory(&mut before);
                    normalize_terminal_inventory(&mut after);
                    observation_coherent = before == after
                        && restore_inventory_is_coherent(
                            workspace,
                            &allowed_sessions,
                            &after,
                            &inventory,
                        );
                    terminals = Ok(after);
                    agents = Ok(inventory);
                    if observation_coherent {
                        break;
                    }
                }
                (before, agent_attempt, after) => {
                    terminals = match (before, after) {
                        (Err(error), _) | (_, Err(error)) => Err(error),
                        (Ok(_), Ok(after)) => Ok(after),
                    };
                    agents = agent_attempt;
                }
            }
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(25_u64 << attempt));
            }
        }
        let _ = sender.send(RestoreCompletion {
            port,
            dispatched_interaction,
            dispatched_registry_revision,
            dispatched_allowed_sessions: allowed_sessions,
            terminals,
            agents,
            observation_coherent,
        });
    });
}

fn normalize_terminal_inventory(entries: &mut Vec<TerminalInventoryEntry>) {
    entries.sort_by_key(|entry| {
        (
            terminal_restore_sort_key(&entry.terminal),
            match entry.kind {
                TerminalKind::Agent => 0_u8,
                TerminalKind::Terminal => 1_u8,
            },
            entry.live,
        )
    });
    entries.dedup();
}

fn restore_inventory_is_coherent(
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
    terminals: &[TerminalInventoryEntry],
    agents: &AgentInventory,
) -> bool {
    if agents.workspace_id != workspace {
        return false;
    }
    let in_scope = |terminal: &TerminalRef| {
        terminal.workspace_id == workspace
            && terminal
                .session_id
                .is_none_or(|session| allowed_sessions.contains(&session))
    };
    let live_agent_entries = terminals
        .iter()
        .filter(|entry| entry.live && entry.kind == TerminalKind::Agent)
        .filter(|entry| in_scope(&entry.terminal))
        .collect::<Vec<_>>();
    if terminals.iter().any(|entry| !in_scope(&entry.terminal)) {
        return false;
    }
    if agents
        .runtimes
        .iter()
        .any(|item| !in_scope(&item.runtime.terminal))
    {
        return false;
    }
    if terminals.iter().enumerate().any(|(index, entry)| {
        terminals[index + 1..]
            .iter()
            .any(|other| entry.terminal.fences(&other.terminal))
    }) {
        return false;
    }
    let live_runtimes = agents
        .runtimes
        .iter()
        .filter(|item| item.state == AgentRuntimeInventoryState::Live)
        .filter(|item| in_scope(&item.runtime.terminal))
        .collect::<Vec<_>>();
    if live_runtimes.iter().enumerate().any(|(index, item)| {
        live_runtimes[index + 1..]
            .iter()
            .any(|other| other.continuation == item.continuation)
    }) {
        return false;
    }
    live_agent_entries.iter().all(|entry| {
        live_runtimes
            .iter()
            .filter(|item| item.runtime.terminal.fences(&entry.terminal))
            .count()
            == 1
    }) && live_runtimes.iter().all(|item| {
        live_agent_entries
            .iter()
            .filter(|entry| entry.terminal.fences(&item.runtime.terminal))
            .count()
            == 1
    })
}

fn pane_restore_targets(
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
    agents: AgentTabProjection,
    terminals: &[TerminalInventoryEntry],
    current_selected: Option<&TerminalRef>,
) -> Vec<PaneRestoreTarget> {
    let mut targets: BTreeMap<
        Option<SessionId>,
        (
            Vec<crate::usecase::application::pane::LivePane>,
            Option<TerminalRef>,
        ),
    > = BTreeMap::new();
    for target in agents.targets {
        let selected = target.selected.and_then(|selected| {
            target
                .tabs
                .iter()
                .find(|slot| slot.continuation == selected)
                .map(|slot| slot.terminal.clone())
        });
        let entry = targets.entry(target.session_id).or_default();
        entry.0.extend(target.tabs.into_iter().map(|slot| {
            crate::usecase::application::pane::LivePane {
                terminal: slot.terminal,
                kind: PaneKind::Agent,
            }
        }));
        entry.1 = selected;
    }
    targets.entry(None).or_default();
    for session in allowed_sessions {
        targets.entry(Some(*session)).or_default();
    }

    let mut generic = terminals
        .iter()
        .filter(|entry| entry.live && entry.kind == TerminalKind::Terminal)
        .filter(|entry| entry.terminal.workspace_id == workspace)
        .filter(|entry| {
            entry
                .terminal
                .session_id
                .is_none_or(|session| allowed_sessions.contains(&session))
        })
        .cloned()
        .collect::<Vec<_>>();
    generic.sort_by_key(|entry| terminal_restore_sort_key(&entry.terminal));
    for entry in generic {
        let target = targets.entry(entry.terminal.session_id).or_default();
        if !target
            .0
            .iter()
            .any(|pane| pane.terminal.fences(&entry.terminal))
        {
            target.0.push(crate::usecase::application::pane::LivePane {
                terminal: entry.terminal,
                kind: PaneKind::Terminal,
            });
        }
    }
    targets
        .into_iter()
        .map(|(session, (panes, selected))| {
            let selected = selected
                .or_else(|| {
                    current_selected
                        .filter(|terminal| terminal.session_id == session)
                        .filter(|terminal| panes.iter().any(|pane| pane.terminal.fences(terminal)))
                        .cloned()
                })
                .or_else(|| {
                    panes
                        .iter()
                        .find(|pane| pane.kind == PaneKind::Terminal)
                        .or_else(|| panes.first())
                        .map(|pane| pane.terminal.clone())
                });
            PaneRestoreTarget {
                target: session.map_or(Target::Root(workspace), Target::Session),
                panes,
                selected,
            }
        })
        .collect()
}

fn terminal_restore_sort_key(terminal: &TerminalRef) -> (String, String, String, String, String) {
    (
        terminal.daemon_generation.as_str(),
        terminal.terminal_id.as_str(),
        terminal.workspace_id.as_str(),
        terminal
            .session_id
            .map_or_else(String::new, |id| id.as_str()),
        terminal.worktree_id.as_str(),
    )
}

/// Project only generic additions when Agent intent persistence is unavailable.
/// The append-only runtime path preserves all existing panes and selection; a
/// later successful coherent observation owns authoritative membership/order.
fn generic_restore_targets(
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
    terminals: &[TerminalInventoryEntry],
    runtime: &WorkspaceRuntime,
) -> Vec<PaneRestoreTarget> {
    let focused = runtime.focused_terminal();
    pane_restore_targets(
        workspace,
        allowed_sessions,
        AgentTabProjection::default(),
        terminals,
        focused.as_ref(),
    )
    .into_iter()
    .filter(|target| !target.panes.is_empty())
    .collect()
}

fn apply_restore_completion(
    completion: RestoreCompletion,
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
) -> RestoreApply {
    let RestoreCompletion {
        port,
        dispatched_interaction,
        dispatched_registry_revision,
        dispatched_allowed_sessions,
        terminals,
        agents,
        observation_coherent,
    } = completion;
    // A partial or cross-RPC-inconsistent observation is an outage outcome even
    // when the user also moved the runtime fence. Transport failure must keep
    // controller backoff/notice semantics and cannot be converted into an
    // immediate fence retry by key activity.
    if !observation_coherent || terminals.is_err() || agents.is_err() {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::TransportFailed,
        };
    }
    if dispatched_allowed_sessions != *allowed_sessions {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::FenceRejected,
        };
    }
    if runtime.restore_fence() != (dispatched_interaction, dispatched_registry_revision) {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::FenceRejected,
        };
    }
    let terminals = terminals.expect("coherent restore checked terminal transport");
    let agents = agents.expect("coherent restore checked Agent transport");
    let observation = match ui.observe_agent_tabs(terminals.clone(), agents) {
        Ok(observation) => observation,
        Err(error) => {
            let targets = generic_restore_targets(workspace, allowed_sessions, &terminals, runtime);
            let _ = runtime.append_restore_snapshot(
                dispatched_interaction,
                dispatched_registry_revision,
                targets,
            );
            return RestoreApply {
                port,
                outcome: RestoreJobOutcome::IntentFailed(error),
            };
        }
    };
    runtime.set_reopen_choices(ui.agent_reopen_choices());
    if !observation.cas_accepted {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::FenceRejected,
        };
    }
    let selected = runtime.focused_terminal();
    let targets = pane_restore_targets(
        workspace,
        allowed_sessions,
        observation.projection,
        &terminals,
        selected.as_ref(),
    );
    let fence_accepted = runtime.restore_snapshot(
        dispatched_interaction,
        dispatched_registry_revision,
        targets,
    );
    debug_assert!(
        fence_accepted,
        "restore fence cannot change during synchronous intent projection"
    );
    RestoreApply {
        port,
        outcome: RestoreJobOutcome::Applied,
    }
}

#[cfg(test)]
fn restore_open_panes(ui: &mut WorkspaceUi, runtime: &mut WorkspaceRuntime, geometry: Geometry) {
    let Ok(entries) = ui.list_open_terminals() else {
        return;
    };
    let mut grouped: BTreeMap<Option<SessionId>, Vec<crate::usecase::application::pane::LivePane>> =
        BTreeMap::new();
    for entry in entries.iter().filter(|entry| entry.live) {
        let panes = grouped.entry(entry.terminal.session_id).or_default();
        if !panes
            .iter()
            .any(|pane| pane.terminal.fences(&entry.terminal))
        {
            panes.push(crate::usecase::application::pane::LivePane {
                terminal: entry.terminal.clone(),
                kind: match entry.kind {
                    TerminalKind::Agent => PaneKind::Agent,
                    TerminalKind::Terminal => PaneKind::Terminal,
                },
            });
        }
    }
    let workspace = ui
        .agent
        .as_ref()
        .map_or(WorkspaceId::new(), |agent| agent.workspace);
    let targets = grouped
        .into_iter()
        .map(|(session, panes)| PaneRestoreTarget {
            target: session.map_or(Target::Root(workspace), Target::Session),
            selected: panes.first().map(|pane| pane.terminal.clone()),
            panes,
        })
        .collect();
    let (interaction, revision) = runtime.restore_fence();
    let _ = runtime.restore_snapshot(interaction, revision, targets);
    for target in entries.into_iter().filter(|entry| entry.live) {
        ui.start_terminal_session(target.terminal, geometry);
    }
}

/// Close the focused pane tab (Ctrl-O x / Ctrl-O Ctrl-X) and perform the daemon transport work
/// the runtime reports: detach a live subscription, or drop a still-pending
/// launch (both its queued work and its completion routing) so it cannot spawn a
/// detached daemon terminal behind the vanished placeholder.
fn close_focused_terminal_pane(
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    let dismissed = runtime
        .focused_terminal()
        .as_ref()
        .and_then(|terminal| ui.agent_continuation_for(terminal));
    if let Some(continuation) = dismissed {
        let selected = runtime
            .terminal_after_close()
            .flatten()
            .as_ref()
            .and_then(|terminal| ui.agent_continuation_for(terminal));
        if let Err(error) = ui.mutate_agent_intent(AgentTabIntentMutation::DismissAndSelect {
            continuation,
            session_id: runtime.panes().active().session_id(),
            selected,
        }) {
            surface_agent_tab_intent_error(runtime, error);
            return;
        }
        runtime.set_reopen_choices(ui.agent_reopen_choices());
    }
    let outcome = runtime.close_focused_pane();
    if let Some(terminal) = outcome.detach {
        ui.close_terminal(&terminal);
    }
    if let Some(operation) = outcome.cancel {
        pending_targets.remove(&operation);
        let mut found = None;
        for (index, launch) in ui.pane_launches.iter().enumerate() {
            let queued = match launch {
                PaneLaunch::Agent { operation, .. } | PaneLaunch::Terminal { operation, .. } => {
                    *operation
                }
            };
            if queued == operation {
                found = Some(index);
                break;
            }
        }
        if let Some(index) = found {
            ui.pane_launches.remove(index);
        }
    }
}

fn surface_agent_tab_intent_error(runtime: &mut WorkspaceRuntime, error: AgentTabIntentError) {
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(Notice::new(
        error.safe_message(),
    ))));
}

/// Drive a terminal-output pointer gesture. A drag begins or extends a selection
/// against the visible cells. A release copies a non-empty selection to the OS
/// clipboard; a plain click that produced no selection instead opens the
/// `http(s)` URL under the pointer in the browser (#389) — the two gestures are
/// mutually exclusive, so a drag-to-copy never also opens a link. `rows_len` /
/// `scroll` describe the frame's projected viewport so the pointer maps back to
/// the exact retained cell.
#[allow(clippy::too_many_arguments)]
fn handle_terminal_pointer(
    ui: &WorkspaceUi,
    runtime: &WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    browser: &mut dyn BrowserOpener,
    height: usize,
    width: usize,
    rows_len: usize,
    scroll: usize,
    pointer: PointerEvent,
) {
    match pointer.kind {
        PointerKind::Drag => {
            let Some(terminal) = runtime.focused_terminal() else {
                return;
            };
            let Some(point) =
                terminal_point_at(height, width, rows_len, scroll, pointer.column, pointer.row)
            else {
                return;
            };
            if controls.is_dragging() {
                controls.extend_selection(point);
            } else if let Some(cells) = ui.terminal_cells(&terminal) {
                controls.begin_selection(TerminalSelection::begin(cells, point));
            }
        }
        PointerKind::Up => {
            // Releasing the mouse copies the selection but keeps it highlighted:
            // the range stays on screen until a new drag replaces it.
            if let Some(text) = controls.finish_drag() {
                let result = term.copy_text(&text);
                controls.record_copy(&text, result);
                return;
            }
            // No selection was drawn, so this release is a plain click: open the
            // link under it, if any. A click off any link is a harmless no-op.
            let Some(terminal) = runtime.focused_terminal() else {
                return;
            };
            let Some(point) =
                terminal_point_at(height, width, rows_len, scroll, pointer.column, pointer.row)
            else {
                return;
            };
            if let Some(cells) = ui.terminal_cells(&terminal) {
                controls.open_link_at(&cells, point, browser);
            }
        }
    }
}

/// Copy the retained terminal selection, if any, and leave its highlight in
/// place so the same output can be copied repeatedly.
fn copy_terminal_selection(controls: &mut LiveTerminalControls, term: &mut dyn Terminal) {
    let Some(selection) = controls.selection() else {
        controls.set_feedback("no terminal text is selected");
        return;
    };
    let text = selection.text();
    if text.is_empty() {
        controls.set_feedback("no terminal text is selected");
        return;
    }
    let result = term.copy_text(&text);
    controls.record_copy(&text, result);
}

/// Begin a terminal selection when a normal left click lands in the live
/// terminal's rendered content viewport. This records the press cell as the
/// drag anchor, before crossterm delivers the first [`PointerKind::Drag`] event.
/// Sidebar, chrome, modal, and out-of-content clicks retain their existing
/// ownership and handling.
#[allow(clippy::too_many_arguments)]
fn begin_terminal_selection_on_click(
    ui: &WorkspaceUi,
    runtime: &WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    height: usize,
    width: usize,
    rows_len: usize,
    scroll: usize,
    pointer: (u16, u16),
) -> bool {
    if !runtime.wants_live_input() {
        return false;
    }
    let terminal = runtime
        .focused_terminal()
        .expect("live input ownership requires a selected live terminal");
    let Some(point) = terminal_point_at(height, width, rows_len, scroll, pointer.0, pointer.1)
    else {
        return false;
    };
    let Some(cells) = ui.terminal_cells(&terminal) else {
        return false;
    };
    controls.begin_selection(TerminalSelection::begin(cells, point));
    true
}

/// Intercept the live-terminal view controls the Home reducer does not own —
/// copy, scroll, tab close, and pointer drag — returning `true` when the key was
/// consumed here so the shell loop skips reducer dispatch. `rows_len` / `scroll`
/// describe the frame's projected viewport for pointer mapping.
#[allow(clippy::too_many_arguments)]
fn intercept_live_terminal_control(
    key: &Key,
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    browser: &mut dyn BrowserOpener,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    height: usize,
    width: usize,
    rows_len: usize,
    scroll: usize,
) -> bool {
    match key {
        Key::Live(LiveTerminalAction::ScrollUp) => controls.scroll_up(),
        Key::Live(LiveTerminalAction::ScrollDown) => controls.scroll_down(),
        Key::Live(LiveTerminalAction::CloseTab) => {
            close_focused_terminal_pane(ui, runtime, pending_targets);
        }
        Key::Live(LiveTerminalAction::MoveTabNext) => {
            let direction = crate::usecase::application::controller::TabDirection::Next;
            let current = runtime
                .active_pane()
                .tabs()
                .iter()
                .filter_map(|tab| match tab {
                    PaneTab::Live(pane) => Some(pane.terminal.clone()),
                    PaneTab::Pending(_) | PaneTab::Ready(_) => None,
                })
                .collect::<Vec<_>>();
            let next = runtime.terminal_order_after_reorder(direction);
            match ui.persist_agent_order(runtime.panes().active().session_id(), &current, &next) {
                Ok(()) => {
                    let _ = runtime.reorder_tab(direction);
                }
                Err(error) => surface_agent_tab_intent_error(runtime, error),
            }
        }
        Key::Live(LiveTerminalAction::MoveTabPrevious) => {
            let direction = crate::usecase::application::controller::TabDirection::Previous;
            let current = runtime
                .active_pane()
                .tabs()
                .iter()
                .filter_map(|tab| match tab {
                    PaneTab::Live(pane) => Some(pane.terminal.clone()),
                    PaneTab::Pending(_) | PaneTab::Ready(_) => None,
                })
                .collect::<Vec<_>>();
            let next = runtime.terminal_order_after_reorder(direction);
            match ui.persist_agent_order(runtime.panes().active().session_id(), &current, &next) {
                Ok(()) => {
                    let _ = runtime.reorder_tab(direction);
                }
                Err(error) => surface_agent_tab_intent_error(runtime, error),
            }
        }
        Key::Pointer(pointer) => {
            handle_terminal_pointer(
                ui, runtime, controls, term, browser, height, width, rows_len, scroll, *pointer,
            );
        }
        Key::Click { column, row } => {
            return begin_terminal_selection_on_click(
                ui,
                runtime,
                controls,
                height,
                width,
                rows_len,
                scroll,
                (*column, *row),
            );
        }
        _ => return false,
    }
    true
}

/// Compose the controller Home frame: `render_home` plus the shell overlays that
/// `render_home` does not own (create form, quit confirmation).
#[allow(clippy::too_many_arguments)]
fn render_controller_frame(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    root_cwd: &Path,
    sessions: &[ProjectedSession],
    metrics: Option<usagi_core::usecase::client::DaemonMetrics>,
    git_diffs: &BTreeMap<SessionId, GitDiff>,
    terminal_view: Option<TerminalViewProjection>,
    create_pending: Option<&str>,
) -> Vec<String> {
    let projection =
        HomeProjection::from_state(runtime.state(), workspace_name, root_cwd, sessions)
            .with_pane(runtime.active_pane())
            .with_metrics(metrics)
            .with_git_diffs(git_diffs)
            .with_terminal_view(terminal_view)
            .with_create_pending(create_pending.map(str::to_owned))
            .with_overlay_modals(
                runtime.overview_modal().cloned(),
                runtime.closeup_modal().cloned(),
            );
    let frame = render_home(height, width, &projection);
    // The create form renders inline in the `+ new session` sidebar row (see
    // `render_home`), so no overlay composite is needed here.
    if runtime.state().overlay() == Some(Overlay::QuitConfirmation) {
        return quit_modal::render_over(
            height,
            width,
            &frame,
            runtime.state().quit_confirm_selected(),
        );
    }
    // The create-failure dialog carries its safe message exactly while its
    // overlay is open, so keying off the message avoids an unreachable
    // "error overlay without a message" branch.
    if let Some(error) = runtime.state().create_session_error() {
        return create_session_error_modal::render_over(height, width, &frame, &error.message);
    }
    frame
}

/// Apply actions already routed by [`DaemonBackend`] to the stateful terminal
/// host. This layer owns no Effect matching and therefore cannot diverge from
/// the backend's route matrix.
#[allow(clippy::too_many_lines)]
fn drain_controller_host_actions(
    actions: &Receiver<ControllerHostAction>,
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    while let Ok(action) = actions.try_recv() {
        match action {
            ControllerHostAction::Create(request, completions) => {
                let name = request.intent.name;
                let before = ui.workspace.session_ids().to_vec();
                if begin_session_command(
                    ui,
                    SessionCommand::Create { name: name.clone() },
                    SessionBackendCompletion::Create {
                        token: request.token,
                        before,
                        completions,
                    },
                ) {
                    ui.creating_session = Some(PendingCreate { name });
                }
            }
            ControllerHostAction::Refresh(_, completions) => {
                let _ = begin_session_command(
                    ui,
                    SessionCommand::List,
                    SessionBackendCompletion::Refresh {
                        before: ui.workspace.session_ids().to_vec(),
                        completions,
                    },
                );
            }
            ControllerHostAction::Remove(request, completions) => {
                if let Some(name) = session_name_for(ui, request.session) {
                    let before = ui.workspace.session_ids().to_vec();
                    if begin_session_command(
                        ui,
                        SessionCommand::Remove {
                            name,
                            force: request.force,
                        },
                        SessionBackendCompletion::Remove {
                            session: request.session,
                            before,
                            completions,
                        },
                    ) {
                        ui.removing_session = Some(request.session);
                    }
                } else {
                    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                        "selected session is no longer available",
                    ))));
                }
            }
            ControllerHostAction::LaunchAgent(request) => {
                let target = request
                    .session
                    .map_or(Target::Root(request.workspace), Target::Session);
                pending_targets.insert(request.operation_id, target);
                runtime.on_effect(&Effect::LaunchAgent {
                    workspace: request.workspace,
                    session: request.session,
                    operation_id: request.operation_id,
                    profile: request.profile.clone(),
                });
                ui.pane_launches.push(PaneLaunch::Agent {
                    operation: request.operation_id,
                    workspace: request.workspace,
                    session: request.session,
                    profile: request.profile,
                    resume: false,
                });
            }
            ControllerHostAction::ResumeAgent(request) => {
                let target = Target::Session(request.session);
                pending_targets.insert(request.operation_id, target);
                runtime.on_effect(&Effect::LaunchAgent {
                    workspace: request.workspace,
                    session: Some(request.session),
                    operation_id: request.operation_id,
                    profile: None,
                });
                ui.pane_launches.push(PaneLaunch::Agent {
                    operation: request.operation_id,
                    workspace: request.workspace,
                    session: Some(request.session),
                    profile: None,
                    resume: true,
                });
            }
            ControllerHostAction::ReopenAgent(request) => {
                if ui
                    .agent
                    .as_ref()
                    .is_some_and(|agent| request.workspace == agent.workspace)
                {
                    let reopened = ui.mutate_agent_intent(AgentTabIntentMutation::Reopen {
                        continuation: request.continuation,
                    });
                    match reopened {
                        Ok(()) => {
                            ui.request_agent_observation();
                            runtime.set_reopen_choices(ui.agent_reopen_choices());
                            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                                Notice::new(
                                    "Agent reopen was saved; waiting for daemon observation",
                                ),
                            )));
                        }
                        Err(error) => surface_agent_tab_intent_error(runtime, error),
                    }
                }
            }
            ControllerHostAction::OpenTerminal(request) => {
                // A terminal opens for any target, including the workspace root; the
                // daemon resolves the root scope to the trusted repository root.
                if let Some(agent) = ui.agent.as_ref() {
                    let workspace = agent.workspace;
                    pending_targets.insert(request.operation_id, request.target);
                    runtime.on_effect(&Effect::OpenTerminal {
                        target: request.target,
                        operation_id: request.operation_id,
                        arguments: request.arguments.clone(),
                    });
                    ui.pane_launches.push(PaneLaunch::Terminal {
                        operation: request.operation_id,
                        workspace,
                        session: request.target.session_id(),
                        arguments: request.arguments,
                    });
                }
            }
            ControllerHostAction::OpenExternalTerminal(target) => {
                let path = match target {
                    Target::Root(_) => Some(ui.workspace.path().to_path_buf()),
                    Target::Session(session) => ui
                        .workspace
                        .sessions()
                        .iter()
                        .zip(ui.workspace.session_ids())
                        .find(|(_, id)| **id == session)
                        .map(|(record, _)| record.root.clone()),
                };
                match path {
                    Some(path) => {
                        if let Err(error) = ui.external_terminal.open(&path) {
                            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                                Notice::new(error),
                            )));
                        }
                    }
                    None => {
                        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                            Notice::new("selected session is no longer available"),
                        )));
                    }
                }
            }
            ControllerHostAction::SelectTab(direction) => {
                let active = runtime.panes().active();
                let Some(next_terminal) = runtime.terminal_after_select(direction) else {
                    continue;
                };
                if !ui.has_agent_intent_for(active.session_id()) {
                    runtime.on_effect(&Effect::SelectTab { direction });
                    continue;
                }
                let continuation = next_terminal
                    .as_ref()
                    .and_then(|terminal| ui.agent_continuation_for(terminal));
                match ui.mutate_agent_intent(AgentTabIntentMutation::Select {
                    session_id: active.session_id(),
                    continuation,
                }) {
                    Ok(()) => runtime.on_effect(&Effect::SelectTab { direction }),
                    Err(error) => surface_agent_tab_intent_error(runtime, error),
                }
            }
        }
    }
}

/// Apply completed pane launches: promote and focus the runtime tab, then attach
/// the daemon terminal stream, so the live viewport renders next frame.
fn drain_pane_completions_into_runtime(
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    _geometry: Geometry,
) {
    while let Ok(completion) = ui.pane_completions.try_recv() {
        if let Some(agent) = ui.agent.as_mut() {
            agent.port = Some(completion.port);
        }
        match completion.outcome {
            PaneLaunchOutcome::Agent { operation, result } => {
                let Some(target) = pending_targets.remove(&operation) else {
                    continue;
                };
                match result {
                    Ok(admission) => {
                        let terminal = admission.terminal;
                        if let Some(continuation) = admission.continuation {
                            let select = runtime.pane_completion_will_focus(operation);
                            match ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
                                session_id: target.session_id(),
                                continuation,
                                terminal: terminal.clone(),
                                select,
                            }) {
                                Ok(()) => {
                                    let _ = runtime.complete_pane_focus_if_uninterrupted(
                                        target, operation, terminal,
                                    );
                                }
                                Err(error) => {
                                    let _ = runtime.fail_pane(
                                        target,
                                        operation,
                                        error.safe_message().to_owned(),
                                    );
                                    surface_agent_tab_intent_error(runtime, error);
                                }
                            }
                            runtime.set_reopen_choices(ui.agent_reopen_choices());
                        } else {
                            let _ = runtime
                                .complete_pane_focus_if_uninterrupted(target, operation, terminal);
                        }
                    }
                    Err(message) => {
                        let _ = runtime.fail_pane(target, operation, message);
                    }
                }
            }
            PaneLaunchOutcome::Terminal { operation, result } => {
                let Some(target) = pending_targets.remove(&operation) else {
                    continue;
                };
                match result {
                    Ok(terminal) => {
                        let _ = runtime
                            .complete_pane_focus_if_uninterrupted(target, operation, terminal);
                    }
                    Err(message) => {
                        let _ = runtime.fail_pane(target, operation, message);
                    }
                }
            }
        }
    }
}

/// Build the controller event for a sidebar click. The shell supplies the raw
/// cell and an injected monotonic timestamp; stable identity and double-click
/// detection remain controller responsibilities.
fn sidebar_pointer_event(column: u16, row: u16, at: std::time::Duration) -> AppEvent {
    AppEvent::Pointer { column, row, at }
}

/// Controller-driven real-terminal frame loop (`drain → poll → render → input →
/// dispatch`). Home row state, live-pane availability, and the Home frame come
/// from [`WorkspaceRuntime`]/`render_home`; the legacy [`WorkspaceUi`] is kept as
/// the daemon IO transport (session workers, pane launches, terminal streams,
/// metrics). This is the controller replacement for
/// `drive_workspace_with_agent_port_and_selection_mode`; the composition root
/// switches to it separately.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
fn drive_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
    modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode,
    mut workspace_config: Option<WorkspaceConfigContext<'_>>,
) -> io::Result<WorkspaceStep> {
    let workspace_id = snapshot.workspace_id;
    let session_ids = snapshot.session_ids.clone();
    let workspace_name = snapshot.workspace.name.clone();
    let root_cwd = snapshot.workspace.path.clone();
    let agent_resumes = snapshot.agent_resumes.clone();
    let (host, host_rx) = ControllerHost::channel();
    let composition = backend_factory.create(&snapshot, host);
    let mut backend = composition.backend;
    let mut browser = composition.browser;
    let mut restore_commands = Some(composition.restore_commands);
    let mut restore_connection = composition.restore_connection;
    let (restore_sender, restore_completions) = mpsc::channel();
    let workspace =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, session_ids.clone());
    let mut ui = WorkspaceUi::new(workspace, composition.session_commands)
        .with_agent_resumes(agent_resumes)
        .with_agent_context(
            workspace_id,
            session_ids.clone(),
            composition.agent_commands,
        )
        .with_agent_tab_intent(
            workspace_id,
            session_ids.iter().copied().collect(),
            composition.agent_tab_intents,
        )
        .with_external_terminal(composition.external_terminal);
    let mut runtime =
        WorkspaceRuntime::with_selection_mode(workspace_id, session_ids, modal_selection_mode);
    if let Some(error) = ui.take_agent_tab_intent_load_error() {
        surface_agent_tab_intent_error(&mut runtime, error);
    }
    runtime.set_reopen_choices(ui.agent_reopen_choices());
    let mut metrics_backend = MetricsBackend::new(composition.metrics);
    let mut metrics_projection = MetricsProjection::default();
    let mut pending_targets: std::collections::HashMap<OperationId, Target> =
        std::collections::HashMap::new();
    // The reducer hit-tests sidebar clicks and owns stable-identity double-click
    // state. The shell's clock is reduced to a deterministic elapsed timestamp.
    let pointer_clock = std::time::Instant::now();
    // Live-terminal scroll offset, drag selection, and copy feedback the reducer
    // does not own (design §4.2).
    let mut controls = LiveTerminalControls::default();
    // Seed the daemon-authoritative snapshot before the first frame so a
    // pending decision is visible without requiring a manual key binding.
    let _ = backend.dispatch(Effect::RefreshDecisions {
        workspace: workspace_id,
    });
    // Start restore after the first frame. The controller owns retry admission
    // and a capped backoff across worker jobs; a frame tick never resets it.
    let restore_clock = std::time::Instant::now();
    let mut restore_retry = RestoreRetryState::new();
    loop {
        for event in backend.drain_events() {
            let _ = runtime.apply_event(event);
        }
        while let Some(epoch) = restore_connection.take_reconnected_epoch() {
            restore_retry.reconnected(epoch, restore_clock.elapsed());
        }
        drain_controller_host_actions(&host_rx, &mut ui, &mut runtime, &mut pending_targets);
        if ui.take_agent_observation_request() {
            restore_retry.request_observation(restore_clock.elapsed());
        }
        drain_session_completions(&mut ui);
        sync_runtime_sessions(&mut runtime, &ui);
        let current_sessions = ui
            .workspace
            .session_ids()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        ui.set_allowed_agent_sessions(current_sessions.iter().copied());
        while let Ok(completion) = restore_completions.try_recv() {
            let applied = apply_restore_completion(
                completion,
                &mut ui,
                &mut runtime,
                workspace_id,
                &current_sessions,
            );
            let outcome = applied.outcome;
            let show_notice = restore_retry.complete(restore_clock.elapsed(), outcome);
            restore_commands = Some(applied.port);
            if show_notice {
                let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                    "daemon restore is unavailable after retries; no Agent was started",
                ))));
            }
            if let RestoreJobOutcome::IntentFailed(error) = outcome {
                surface_agent_tab_intent_error(&mut runtime, error);
            }
        }
        let (height, width) = term.size()?;
        ui.set_terminal_size(height, width);
        let _ = runtime.apply_event(AppEvent::Resize {
            width: u16::try_from(width).unwrap_or(u16::MAX),
            height: u16::try_from(height).unwrap_or(u16::MAX),
        });
        let geometry = terminal_geometry(height, width);
        drain_pane_completions_into_runtime(&mut ui, &mut runtime, &mut pending_targets, geometry);
        ui.sync_foreground_terminal(runtime.focused_terminal().as_ref(), geometry);
        ui.resize_terminals(geometry);
        let (terminal_view, terminal_rows_len, terminal_scroll) =
            poll_and_project_terminals(&mut ui, &mut runtime, &mut controls, geometry);
        if ui.take_terminal_reconnected() {
            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Feedback(
                Feedback::Reconnected,
            )));
        }
        let sessions = project_controller_sessions(&ui);
        // Reflux daemon metrics / git diffs through the backend drain instead of
        // polling the port inline: the shell folds the updates into its own
        // projection cache, so the material no longer rides on the legacy view.
        let metrics_sessions = sessions
            .iter()
            .map(|session| (session.id, session.cwd.clone()))
            .collect::<Vec<_>>();
        metrics_backend.poll(&metrics_sessions);
        for update in metrics_backend.drain_events() {
            metrics_projection.apply(update);
        }
        let frame = render_controller_frame(
            height,
            width,
            &runtime,
            &workspace_name,
            &root_cwd,
            &sessions,
            metrics_projection.metrics(),
            metrics_projection.git_diffs(),
            terminal_view.clone(),
            ui.creating_session
                .as_ref()
                .map(|create| create.name.as_str()),
        );
        term.draw(&frame)?;
        if restore_commands.is_some() && restore_retry.begin_if_due(restore_clock.elapsed()) {
            let port = restore_commands
                .take()
                .expect("restore admission checked the dedicated port");
            let (interaction, registry_revision) = runtime.restore_fence();
            spawn_restore_job(
                port,
                workspace_id,
                current_sessions.clone(),
                interaction,
                registry_revision,
                restore_sender.clone(),
            );
        }
        drain_pane_launches(&mut ui, geometry);
        let key = term.read_key()?;
        // A tick is a bounded UI/session refresh point. Restore retry admission
        // is clocked above and an explicit Reconnected event starts a new epoch;
        // this wakeup never issues inventory RPCs by itself.
        if matches!(key, Key::Other) {
            let _ = backend.dispatch(Effect::RefreshDecisions {
                workspace: workspace_id,
            });
        }
        if let Some(effect) =
            tick_session_refresh(&key, ui.active_session_command.is_none(), workspace_id)
        {
            let _ = backend.dispatch(effect);
        }
        if forward_live_terminal_input(&mut ui, &runtime, &mut controls, term, &key) {
            continue;
        }
        // Live-terminal view controls the reducer does not own (scroll, tab close,
        // pointer drag / copy — design §4.2) are handled before the key reaches
        // the Home reducer.
        if intercept_live_terminal_control(
            &key,
            &mut ui,
            &mut runtime,
            &mut controls,
            term,
            browser.as_mut(),
            &mut pending_targets,
            height,
            width,
            terminal_rows_len,
            terminal_scroll,
        ) {
            continue;
        }
        let effects = if let Key::Click { column, row } = key {
            // The notice centre occupies the right side of Home's top header.
            // Keep it above the sidebar hit-test so a header click never moves
            // the background selection, and let an open modal retain ownership.
            if row == 0
                && !runtime.state().unread_decision_ids().is_empty()
                && usize::from(column) >= width.saturating_sub(28)
            {
                runtime.apply_event(AppEvent::Key(AppKey::OpenDecisions))
            } else {
                runtime.apply_event(sidebar_pointer_event(column, row, pointer_clock.elapsed()))
            }
        } else {
            runtime.handle_key(key)
        };
        for effect in effects {
            let opens_workspace_config = matches!(
                &effect,
                Effect::WorkspaceCommand {
                    workspace,
                    command: crate::usecase::overview::Command::Config { arguments },
                } if *workspace == workspace_id && arguments.trim().is_empty()
            );
            if opens_workspace_config && let Some(context) = workspace_config.as_mut() {
                let base = render_controller_frame(
                    height,
                    width,
                    &runtime,
                    &workspace_name,
                    &root_cwd,
                    &sessions,
                    metrics_projection.metrics(),
                    metrics_projection.git_diffs(),
                    terminal_view.clone(),
                    ui.creating_session
                        .as_ref()
                        .map(|create| create.name.as_str()),
                );
                run_workspace_config(term, context.settings, context.available_models, &base)?;
                let effective =
                    usagi_core::usecase::settings::read_for_workspace_entry(context.settings);
                runtime.set_modal_selection_mode(effective.modal_selection_mode);
                continue;
            }
            if backend.dispatch(effect) == BackendFlow::Exit {
                return Ok(WorkspaceStep::Quit);
            }
        }
    }
}

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
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        usagi_core::domain::settings::ModalSelectionMode::Action,
        None,
    )
    .map(|_| Exit::Quit)
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
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        settings.modal_selection_mode,
        None,
    )
    .map(|_| Exit::Quit)
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
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        effective.modal_selection_mode,
        Some(WorkspaceConfigContext {
            settings,
            available_models,
        }),
    )
    .map(|_| Exit::Quit)
}

struct FixedBackendFactory {
    sessions: Option<Box<dyn SessionCommandPort>>,
    agent: Option<Box<dyn AgentCommandPort>>,
    restore: Option<Box<dyn AgentCommandPort>>,
    metrics: Option<Box<dyn MetricsPort>>,
    browser: Option<Box<dyn BrowserOpener>>,
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
            .with_decisions(Box::new(UnavailableBackendPort))
            .with_overlay(Box::new(UnavailableBackendPort)),
            session_commands: self
                .sessions
                .take()
                .expect("fixed session port is created once"),
            agent_commands: self.agent.take().expect("fixed agent port is created once"),
            restore_commands: self
                .restore
                .take()
                .unwrap_or_else(|| Box::new(UnavailableAgentCommandPort)),
            restore_connection: Box::new(UnavailableRestoreConnectionPort),
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
        }
    }
}

/// Compatibility entry for embedders that still supply individual host ports.
/// Production uses [`run_workspace_controller_with_backend`].
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
        restore: None,
        metrics: Some(metrics),
        browser: Some(browser),
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

/// Run the Welcome / Open / Recent graph with the daemon Agent launch factory.
///
/// # Errors
///
/// Returns workspace loading or terminal IO failures from the screen graph.
#[allow(clippy::too_many_arguments)]
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
pub fn run_with_settings_and_agent_port_factory(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
    agent_commands: &mut dyn AgentCommandPortFactory,
) -> io::Result<Exit> {
    run_with_settings_and_agent_port_factory_and_model_availability(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        agent_commands,
        AvailableAgentModels::all(),
    )
}

/// Run the screen graph while limiting Config's Agent model choices to installed CLIs.
///
/// # Errors
///
/// Returns workspace loading or terminal IO failures from the screen graph.
#[allow(clippy::too_many_arguments)]
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
pub fn run_with_settings_and_agent_port_factory_and_model_availability(
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
) -> io::Result<Exit> {
    let mut metrics = NoMetricsFactory;
    run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        agent_commands,
        available_models,
        &mut metrics,
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
fn open_snapshot_via_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<WorkspaceStep> {
    settings.select_workspace(&snapshot.workspace.path)?;
    let effective = usagi_core::usecase::settings::read_for_workspace_entry(settings);
    drive_workspace_controller(
        term,
        snapshot,
        backend_factory,
        effective.modal_selection_mode,
        Some(WorkspaceConfigContext {
            settings,
            available_models,
        }),
    )
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
        ControllerBackendComposition {
            backend,
            session_commands: self.sessions.create(),
            agent_commands,
            restore_commands: self.agents.as_deref_mut().map_or_else(
                || -> Box<dyn AgentCommandPort> { Box::new(UnavailableAgentCommandPort) },
                AgentCommandPortFactory::create,
            ),
            restore_connection: Box::new(UnavailableRestoreConnectionPort),
            agent_tab_intents: Box::new(UnavailableAgentTabIntentPort),
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            metrics,
            browser: Box::new(UnavailableBrowserOpener),
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

/// Production screen graph entry. Every Welcome/Open/Recent/New path creates
/// its workspace runtime through the same backend factory as direct launch.
///
/// # Errors
///
/// Returns workspace loading, settings, or terminal IO failures.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    let mut welcome = Welcome::new(recent);
    let mut open = open_from_registry(workspaces, welcome.recent());
    let mut new_form = New::default();
    let mut config_form = Config::load_with_available_models(settings, available_models);
    let mut screen = match start {
        Start::Welcome => Screen::Welcome,
        Start::Config => Screen::Config,
    };
    loop {
        let (height, width) = term.size()?;
        let frame = match screen {
            Screen::Welcome => welcome::render(height, width, &welcome, now),
            Screen::Open => render_open(height, width, &open, now),
            Screen::New => new::render(height, width, &new_form),
            Screen::Config => config::render(height, width, &config_form),
        };
        term.draw(&frame)?;
        let key = term.read_key()?;
        match screen {
            Screen::Welcome => match step_welcome(&mut welcome, key) {
                WelcomeStep::Stay => {}
                WelcomeStep::Quit => return Ok(Exit::Quit),
                WelcomeStep::OpenList => screen = Screen::Open,
                WelcomeStep::NewForm => screen = Screen::New,
                WelcomeStep::ConfigScreen => {
                    config_form = Config::load_with_available_models(settings, available_models);
                    screen = Screen::Config;
                }
                WelcomeStep::OpenRecent(index) => {
                    let Some(path) = welcome
                        .recent()
                        .get(index)
                        .and_then(recent_path)
                        .map(Path::to_path_buf)
                    else {
                        continue;
                    };
                    let snapshot = loader.open(&path)?;
                    welcome.record_opened(&snapshot.workspace);
                    open.record_opened(&snapshot.workspace);
                    let workspace_step = open_snapshot_via_controller(
                        term,
                        snapshot,
                        settings,
                        backend_factory,
                        available_models,
                    );
                    workspace_step?;
                    return Ok(Exit::Quit);
                }
            },
            Screen::Open => match step_open(&mut open, key) {
                OpenStep::Stay => {}
                OpenStep::Quit => return Ok(Exit::Quit),
                OpenStep::Back => screen = Screen::Welcome,
                OpenStep::Choose(path) => {
                    let snapshot = loader.open(&path)?;
                    welcome.record_opened(&snapshot.workspace);
                    open.record_opened(&snapshot.workspace);
                    return open_snapshot_via_controller(
                        term,
                        snapshot,
                        settings,
                        backend_factory,
                        available_models,
                    )
                    .map(|_| Exit::Quit);
                }
                OpenStep::ConfirmCleanup => {
                    let removed = loader.cleanup_missing(&open.workspaces())?;
                    open.remove_paths(&removed);
                }
                OpenStep::ConfirmUnregister(path) => {
                    let removed = loader.unregister(&[path])?;
                    open.remove_paths(&removed);
                }
            },
            Screen::New => match step_new(&mut new_form, key) {
                NewStep::Stay => {}
                NewStep::Quit => return Ok(Exit::Quit),
                NewStep::Back => screen = Screen::Welcome,
                NewStep::Create(request) => match loader.create_workspace(&request) {
                    Ok(snapshot) => {
                        new_form.set_notice(None);
                        welcome.record_opened(&snapshot.workspace);
                        open.record_opened(&snapshot.workspace);
                        return open_snapshot_via_controller(
                            term,
                            snapshot,
                            settings,
                            backend_factory,
                            available_models,
                        )
                        .map(|_| Exit::Quit);
                    }
                    // 失敗時は入力中の draft を保持したまま notice を出して同画面に留まる。
                    Err(error) => new_form.set_notice(Some(new_project_notice(&error))),
                },
            },
            Screen::Config => match step_config(&mut config_form, key, settings) {
                ConfigStep::Stay => {}
                ConfigStep::Quit => return Ok(Exit::Quit),
                ConfigStep::Back => screen = Screen::Welcome,
                ConfigStep::Save => {
                    play_config_save_wave(term, &mut config_form, None)?;
                    if config_form.commit_save(settings) {
                        // Hold the `done` confirmation briefly, then return home
                        // with no key press. A failed write skips this and leaves
                        // Config on screen with the error for retry.
                        let (height, width) = term.size()?;
                        term.draw(&config::render(height, width, &config_form))?;
                        term.wait(config::DONE_DISPLAY)?;
                        config_form.reset_save();
                        screen = Screen::Welcome;
                    }
                }
            },
        }
    }
}

/// v1 と同じ Welcome 起動エフェクトを再生する。入力は読まないため、スプラッシュ中の
/// type-ahead はそのまま Welcome の最初のキー入力へ渡る。
///
/// # Errors
///
/// 端末サイズの取得、描画、フレーム間待機のいずれかに失敗した場合、そのエラーを返す。
pub fn play_startup_splash(term: &mut dyn Terminal) -> io::Result<()> {
    for frame in 0..splash::FRAMES {
        let (height, width) = term.size()?;
        term.draw(&splash::render(height, width, frame))?;
        term.wait(splash::ANIM_TICK)?;
    }
    Ok(())
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

/// 選ばれた TUI 画面を識別できる一行を出力する非対話 runner。
///
/// 出力先とアプリ情報は呼び出し側から注入するため、実 stdout を直接所有しない。
pub struct BannerScreenRunner<'a, W: Write + ?Sized> {
    out: &'a mut W,
    info: &'a AppInfo,
}

impl<'a, W: Write + ?Sized> BannerScreenRunner<'a, W> {
    /// 注入された出力先とアプリ情報から runner を作る。
    #[must_use]
    pub fn new(out: &'a mut W, info: &'a AppInfo) -> Self {
        Self { out, info }
    }

    /// 画面を識別する `label` をアプリ情報とともに一行で書き出す。
    fn write_screen(&mut self, label: &str) -> io::Result<()> {
        writeln!(self.out, "{}: {label}", self.info.describe())
    }
}

impl<W: Write + ?Sized> ScreenRunner for BannerScreenRunner<'_, W> {
    fn welcome(&mut self) -> io::Result<()> {
        self.write_screen("welcome TUI")
    }

    fn workspace(&mut self, path: &Path) -> io::Result<()> {
        self.write_screen(&format!("workspace TUI ({})", path.display()))
    }

    fn config(&mut self) -> io::Result<()> {
        self.write_screen("config TUI")
    }

    fn doctor(&mut self) -> io::Result<()> {
        self.write_screen("doctor TUI")
    }
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use super::{
        AgentCommandPort, AgentCommandPortFactory, AgentPaneAdmission, AgentTabIntentPort,
        AgentTabIntentPortCommit, BannerScreenRunner, BrowserOpener, Config, ConfigStep,
        ControllerHost, ControllerHostAction, DecisionCommandPort, DefaultSettingsPort,
        DesktopNotificationPort, EnvironmentStorePort, Exit, ExternalTerminalPort,
        FixedBackendFactory, Geometry, MetricsPort, MetricsPortFactory, NewStep,
        NoDesktopNotifications, NoMetrics, NoMetricsFactory, OpenStep, PaneLaunch,
        SessionCommandPort, SessionCommandPortFactory, SessionCommandResult, Start, TerminalAttach,
        TerminalChunk, TerminalError, TerminalInputOutcome, UnavailableAgentCommandPort,
        UnavailableBackendPort, UnavailableBrowserOpener, UnavailableDecisionCommandPort,
        UnavailableEnvironmentStore, UnavailableExternalTerminalPort, UnavailablePrSnapshotPort,
        UnavailableSessionCommandPort, UnavailableSessionCommandPortFactory, WelcomeStep,
        WorkspaceLoader, WorkspaceRuntime, WorkspaceSnapshot, WorkspaceUi, WorkspaceView,
        app_event_from_key, begin_terminal_selection_on_click, close_exited_panes,
        controller_terminal_view, copy_terminal_selection, drain_controller_host_actions,
        drain_session_completions, forward_live_terminal_input, handle_terminal_pointer,
        intercept_live_terminal_control, key_to_terminal_bytes, new_project_notice,
        play_startup_splash, poll_and_project_terminals, render_controller_frame,
        render_home_snapshot, restore_open_panes, run as run_from_start, run_with_settings,
        run_with_settings_and_agent_and_metrics_port_factory_and_model_availability,
        run_workspace_config, run_workspace_controller, run_workspace_controller_with_backend,
        run_workspace_controller_with_backend_and_config,
        run_workspace_controller_with_backend_and_settings, safe_session_error,
        session_worktree_names, sidebar_pointer_event, step_config, step_new, step_open,
        terminal_geometry, tick_session_refresh, welcome_action, write_banner,
    };
    use crate::presentation::live_terminal::LiveTerminalControls;
    use crate::presentation::views::config::AvailableAgentModels;
    use crate::presentation::views::new::{Field, Mode, New};
    use crate::presentation::views::open::Open;
    use crate::presentation::views::welcome::MenuAction;
    use crate::presentation::widgets::strip_ansi;
    use crate::usecase::application::agent_tab_intent::{
        AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabProjection,
        AgentTabSlotIntent, AgentTabTargetProjection,
    };
    use crate::usecase::application::controller::{
        AppEvent, AppKey, BackendEvent, Effect, EnvironmentEntry, NewRequest, PendingToken,
        SessionCreateIntent, TabDirection, Target,
    };
    use crate::usecase::application::daemon_backend::{DaemonBackend, ReopenAgentRequest};
    use crate::usecase::application::pane::{LivePane, PaneKind, PaneTab};
    use crate::usecase::application::pr::PrSnapshotPort;
    use crate::usecase::application::run as dispatch;
    use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};
    use crate::usecase::application::{EntryScreen, Key, Terminal};
    use crate::usecase::overview::SessionCommand;
    use crate::usecase::terminal_input::{LiveTerminalAction, PointerEvent, PointerKind};
    use chrono::{DateTime, Duration, Utc};
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
    };
    use usagi_core::domain::id::{
        AgentContinuationRef, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, OperationId,
        SessionId, TerminalId, TerminalRef, UserDecisionId, WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::settings::Settings;
    use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
    use usagi_core::domain::user_decision::UserDecisionAnswer;
    use usagi_core::usecase::settings::{SettingsPort, SettingsScope};

    use usagi_core::domain::recent::{Recent, UniteOverview};
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};

    use tempfile::tempdir;
    use usagi_core::domain::workspace::{Workspace, WorkspaceOverview};
    use usagi_core::domain::workspace_state::WorkspaceState;
    use usagi_core::usecase::client::DaemonMetrics;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn app_event_from_key_maps_ordinary_management_keys() {
        assert_eq!(app_event_from_key(Key::Up), Some(AppEvent::Key(AppKey::Up)));
        assert_eq!(
            app_event_from_key(Key::Down),
            Some(AppEvent::Key(AppKey::Down))
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
    }

    #[test]
    fn app_event_from_key_maps_resolved_live_actions_to_reducer_keys() {
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::Switch)),
            Some(AppEvent::Key(AppKey::CtrlO))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::OpenCloseupModal)),
            Some(AppEvent::Key(AppKey::OpenCloseupOverlay))
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
            app_event_from_key(Key::Live(LiveTerminalAction::Agent)),
            Some(AppEvent::Key(AppKey::CtrlA))
        );
        assert_eq!(
            app_event_from_key(Key::Live(LiveTerminalAction::QuitConfirmation)),
            Some(AppEvent::Key(AppKey::OpenQuitConfirmation))
        );
    }

    #[test]
    fn app_event_from_key_ticks_on_wakeups_and_drops_pane_only_input() {
        // Resize / backend wakeups reach the loop as `Other` and advance the mascot.
        assert_eq!(app_event_from_key(Key::Other), Some(AppEvent::Tick));
        // Raw passthrough and terminal pointer drags never reach the Home reducer.
        assert_eq!(app_event_from_key(Key::Passthrough(vec![0x1b])), None);
        // A bracketed paste is a live-pane concern; Home navigation ignores it.
        assert_eq!(app_event_from_key(Key::Paste("x".to_owned())), None);
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
        // Tab close and terminal scroll/copy stay pane- and shell-level concerns.
        for action in [
            LiveTerminalAction::CloseTab,
            LiveTerminalAction::ScrollUp,
            LiveTerminalAction::ScrollDown,
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
    fn terminal_wakeup_refreshes_sessions_while_other_commands_are_running() {
        let workspace = WorkspaceId::new();

        assert_eq!(
            tick_session_refresh(&Key::Other, true, workspace),
            Some(Effect::RefreshSessions { workspace })
        );
        assert_eq!(tick_session_refresh(&Key::Enter, true, workspace), None);
        assert_eq!(tick_session_refresh(&Key::Other, false, workspace), None);
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
                environment: std::collections::BTreeMap::new(),
            }],
            root_notes: Scratchpad::default(),
            root_environment: std::collections::BTreeMap::new(),
            updated_at: now(),
        }
    }

    fn snapshot(name: &str) -> WorkspaceSnapshot {
        WorkspaceSnapshot::new(ws(name), state(name))
    }

    #[test]
    fn session_worktree_names_include_stale_directories_only() {
        let temp = tempdir().unwrap();
        let sessions = temp.path().join(".usagi/sessions");
        std::fs::create_dir_all(sessions.join("stale-session")).unwrap();
        std::fs::write(sessions.join("not-a-worktree"), "marker").unwrap();

        assert_eq!(session_worktree_names(temp.path()), vec!["stale-session"]);
    }

    #[test]
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
                    profile: None,
                    model: None,
                },
            },
            Effect::RefreshSessions { workspace },
            Effect::RemoveSession {
                workspace,
                session,
                force: true,
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
        assert_eq!(actions.try_iter().count(), 9);

        for effect in [
            Effect::LoadNotes { target },
            Effect::SaveNotes {
                target,
                scratchpad: Scratchpad::default(),
            },
            Effect::LoadEnvironment { target },
            Effect::SaveEnvironment {
                target,
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
            Effect::LoadPreview { target },
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
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
                    profile: None,
                    model: None,
                },
            },
            Effect::RefreshSessions { workspace },
            Effect::RemoveSession {
                workspace,
                session: SessionId::new(),
                force: false,
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
        drain_controller_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
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
        assert_eq!(events.len(), 3);
        assert!(events.iter().any(|event| matches!(
            event,
            AppEvent::OperationResult(result) if result.token == token && !result.succeeded
        )));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AppEvent::Backend(BackendEvent::Notice(_))))
                .count(),
            2
        );
        assert_eq!(ui.pane_launches.len(), 2);
        assert!(!pending.is_empty());

        let calls = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(SnapshotSessionPort(calls.clone())))
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
        drain_controller_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_session_completions(&mut ui);
        backend.dispatch(Effect::RemoveSession {
            workspace,
            session,
            force: true,
        });
        drain_controller_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_session_completions(&mut ui);
        backend.dispatch(Effect::OpenTerminal {
            target,
            operation_id: OperationId::new(),
            arguments: "new".into(),
        });
        drain_controller_host_actions(&actions, &mut ui, &mut runtime, &mut pending);
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One shell fixture keeps port absence and async completion in sequence.
    fn workspace_shell_harness_covers_port_absence_projection_and_async_launch_completion() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let terminal = live_terminal_ref(workspace, session);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));

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
        super::apply_session_projection(&mut ui, None, None, None);
        super::apply_session_projection(&mut ui, Some(records.clone()), None, None);
        super::apply_session_projection(&mut ui, Some(records), Some(vec![session]), None);
        let records = ui.workspace.sessions().to_vec();
        super::apply_session_projection(
            &mut ui,
            Some(records),
            Some(vec![session]),
            Some(std::collections::BTreeMap::new()),
        );
        let mut mismatched_runtime = WorkspaceRuntime::new(workspace, Vec::new());
        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                port: Box::new(SuccessfulAgentPort(terminal.clone())),
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
        super::sync_runtime_sessions(&mut mismatched_runtime, &ui);
        let mut no_controls = LiveTerminalControls::default();
        let _ = super::poll_and_project_terminals(
            &mut ui,
            &mut mismatched_runtime,
            &mut no_controls,
            Geometry { cols: 20, rows: 5 },
        );
        let mut ui = ui.with_agent_context(
            workspace,
            vec![session],
            Box::new(SuccessfulAgentPort(terminal.clone())),
        );
        assert!(ui.send_terminal_bytes(&terminal, b"missing").is_err());
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        for session in [Some(session), None] {
            ui.pane_launches.push(super::PaneLaunch::Agent {
                operation: OperationId::new(),
                workspace,
                session,
                profile: None,
                resume: true,
            });
            super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
            std::thread::sleep(std::time::Duration::from_millis(10));
            super::drain_pane_completions_into_runtime(
                &mut ui,
                &mut runtime,
                &mut std::collections::HashMap::new(),
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
            resume: false,
        });
        let mut pending = std::collections::HashMap::from([(operation, target)]);
        super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );
        assert!(pending.is_empty());
        ui.resize_terminals(Geometry { cols: 30, rows: 6 });
        let projected_records = ui.workspace.sessions().to_vec();
        super::apply_session_projection(
            &mut ui,
            Some(projected_records),
            Some(vec![session]),
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
        std::thread::sleep(std::time::Duration::from_millis(10));
        super::drain_pane_completions_into_runtime(
            &mut ui,
            &mut runtime,
            &mut pending,
            Geometry { cols: 20, rows: 5 },
        );

        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                port: Box::new(SuccessfulAgentPort(terminal.clone())),
                outcome: super::PaneLaunchOutcome::Agent {
                    operation: OperationId::new(),
                    result: Ok(AgentPaneAdmission {
                        terminal: terminal.clone(),
                        continuation: None,
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
                port: Box::new(SuccessfulAgentPort(terminal.clone())),
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

        ui.pane_completion_sender
            .send(super::PaneLaunchCompletion {
                port: Box::new(SuccessfulAgentPort(terminal.clone())),
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

        ui.agent.as_mut().unwrap().port = None;
        assert!(ui.poll_all_terminals().is_empty());
        ui.pane_launches.push(super::PaneLaunch::Agent {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            profile: None,
            resume: false,
        });
        ui.pane_launches.push(super::PaneLaunch::Terminal {
            operation: OperationId::new(),
            workspace,
            session: Some(session),
            arguments: "open".into(),
        });
        super::drain_pane_launches(&mut ui, Geometry { cols: 20, rows: 5 });
        assert!(ui.pane_launches.len() >= 2);
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Down));
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
        assert_eq!(runtime.panes().active(), Target::Session(session));

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
                port: Box::new(SuccessfulAgentPort(agent_terminal.clone())),
                outcome: super::PaneLaunchOutcome::Agent {
                    operation: agent_operation,
                    result: Ok(AgentPaneAdmission {
                        terminal: agent_terminal.clone(),
                        continuation: Some(continuation),
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
                port: Box::new(SuccessfulAgentPort(agent_terminal.clone())),
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
        super::drain_controller_host_actions(&receiver, &mut ui, &mut runtime, &mut pending);
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
    fn compatibility_ports_fail_explicitly_and_never_silently_succeed() {
        struct DefaultSessionPort;
        impl SessionCommandPort for DefaultSessionPort {}

        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let target = Target::Root(workspace_id);
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
                .launch(workspace_id, None, None)
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
            UnavailableEnvironmentStore.load(target),
            BackendEvent::EnvironmentError { .. }
        ));
        assert!(matches!(
            UnavailableEnvironmentStore.save(target, Vec::new()),
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
        let view =
            WorkspaceView::with_runtime_ids(ws("demo"), WorkspaceState::default(), Vec::new());
        let opened = Arc::new(Mutex::new(Vec::new()));
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_external_terminal(Box::new(RecordingExternalTerminalPort(opened.clone())));
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::OpenExternalTerminal(Target::Root(
                workspace,
            )))
            .unwrap();

        drain_controller_host_actions(
            &receiver,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert_eq!(*opened.lock().unwrap(), vec![PathBuf::from("/tmp/demo")]);
    }

    struct SuccessfulAgentPort(TerminalRef);

    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_screen_graph_fake_port_contract
    impl AgentCommandPort for SuccessfulAgentPort {
        fn launch(
            &mut self,
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Ok(AgentPaneAdmission {
                terminal: self.0.clone(),
                continuation: None,
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
                SessionCommand::Create { name } => Some(vec![SessionRecord {
                    name: name.clone(),
                    display_name: None,
                    origin: SessionOrigin::Human,
                    started_from: None,
                    root: workspace.path.join(".usagi/sessions").join(name),
                    created_at: now(),
                    last_active: None,
                    notes: Scratchpad::default(),
                    prs: Vec::new(),
                    environment: std::collections::BTreeMap::new(),
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
            id: session,
            label: "alpha".into(),
            detail: "fixture".into(),
            cwd: "/work/alpha".into(),
            last_modified: now(),
            has_notes: false,
            pr_summary: None,
            removing: false,
            agent_resume: None,
        };
        let sessions = std::slice::from_ref(&projected);
        let git = std::collections::BTreeMap::new();
        let root = std::path::Path::new("/work");

        // Base Home frame: workspace name and session row render.
        let runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let base = render_controller_frame(
            20, 80, &runtime, "atlas", root, sessions, None, &git, None, None,
        );
        assert!(base.join("\n").contains("atlas"));
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
        let create = render_controller_frame(
            20,
            80,
            &creating,
            "atlas",
            root,
            &[],
            None,
            &git,
            None,
            None,
        );
        assert!(create.join("\n").contains("beta"));
        assert!(!create.join("\n").contains("New session"));

        // Quit confirmation overlay: the shared Yes/No buttons and shortcut line
        // render, defaulting to Yes focused.
        let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = quitting.apply_event(AppEvent::Key(AppKey::CtrlQ));
        let quit = render_controller_frame(
            20, 80, &quitting, "atlas", root, sessions, None, &git, None, None,
        );
        let quit_text = quit.join("\n");
        assert!(quit_text.contains("Detach from this workspace?"));
        assert!(quit_text.contains("[ yes ]"));
        assert!(quit_text.contains("[ no  ]"));
        assert!(quit_text.contains("←→/Tab: choose"));

        // The runtime's persisted Overview palette renders through this path.
        let mut palette = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = palette.handle_key(Key::Char(':'));
        let overview = render_controller_frame(
            20, 80, &palette, "atlas", root, sessions, None, &git, None, None,
        );
        assert!(overview.join("\n").contains("Overview"));

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
        let failure =
            render_controller_frame(20, 80, &failing, "atlas", root, &[], None, &git, None, None);
        assert!(failure.join("\n").contains("Session create failed"));
        assert!(failure.join("\n").contains("worktree path already exists"));
    }

    #[test]
    fn render_controller_frame_draws_a_waving_pending_create_skeleton() {
        // Once a create request is in flight, the shell threads its name here and
        // the sidebar draws a two-line loading skeleton just above `+ new
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
        let root = std::path::Path::new("/work");

        let idle = WorkspaceRuntime::new(workspace, Vec::new());
        let pending = render_controller_frame(
            20,
            80,
            &idle,
            "atlas",
            root,
            &[],
            None,
            &git,
            None,
            Some("beta"),
        );
        let pending_text = strip(&pending);
        assert!(pending_text.contains("+ beta"));
        assert!(pending_text.contains("creating"));

        // No pending create means no skeleton or loading caption.
        let quiet =
            render_controller_frame(20, 80, &idle, "atlas", root, &[], None, &git, None, None);
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
            root,
            &[],
            None,
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
            Box::new(SuccessfulAgentPort(terminal)),
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
                .any(|frame| frame.join("\n").contains("Detach from this workspace?"))
        );
        // Regression: the real Ctrl-Q frame carries the shared Yes/No buttons and
        // the ←→/Tab shortcut, not the old free-text y/n prompt.
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("[ yes ]")),
            "quit confirmation frame is missing the [ yes ] button"
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("[ no  ]")),
            "quit confirmation frame is missing the [ no  ] button"
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("←→/Tab: choose")),
            "quit confirmation frame is missing the choose shortcut"
        );
    }

    struct BlockingRestorePort {
        entered: Sender<()>,
        release: Receiver<()>,
    }

    impl AgentCommandPort for BlockingRestorePort {
        fn launch(
            &mut self,
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
            restore: Some(Box::new(BlockingRestorePort {
                entered: entered_tx,
                release: release_rx,
            })),
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
        };

        let started = std::time::Instant::now();
        let result = run_workspace_controller_with_backend(
            &mut term,
            snapshot("blocked-restore"),
            &mut factory,
        );
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
                .any(|frame| { frame.join("\n").contains("Detach from this workspace?") })
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
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
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
            restore: None,
            metrics: Some(Box::new(NoMetrics)),
            browser: Some(Box::new(UnavailableBrowserOpener)),
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
    fn controller_loop_opens_the_create_form_from_the_new_session_row() {
        // An empty workspace shows only root and `+ new session`, so one Down
        // reaches the create entry deterministically.
        let snapshot = WorkspaceSnapshot::new(
            ws("empty"),
            WorkspaceState {
                sessions: Vec::new(),
                root_notes: Scratchpad::default(),
                root_environment: std::collections::BTreeMap::new(),
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
            Box::new(SuccessfulAgentPort(terminal)),
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
                let SessionCommand::Create { name } = command else {
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
            let snapshot = WorkspaceSnapshot::new(
                ws("empty"),
                WorkspaceState {
                    sessions: Vec::new(),
                    root_notes: Scratchpad::default(),
                    root_environment: std::collections::BTreeMap::new(),
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
                Box::new(SuccessfulAgentPort(terminal)),
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
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
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
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let (first_completions, _) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let (second_completions, _) =
            crate::usecase::application::daemon_backend::Completions::channel();

        assert!(super::begin_session_command(
            &mut ui,
            SessionCommand::List,
            super::SessionBackendCompletion::Refresh {
                before: Vec::new(),
                completions: first_completions,
            },
        ));
        assert!(!super::begin_session_command(
            &mut ui,
            SessionCommand::List,
            super::SessionBackendCompletion::Refresh {
                before: Vec::new(),
                completions: second_completions,
            },
        ));
    }

    #[test]
    fn stale_session_completion_does_not_replace_a_newer_snapshot() {
        let snapshot = snapshot("demo");
        let original = snapshot.session_ids[0];
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
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
                    revision: Some(2),
                }),
                completion: super::SessionBackendCompletion::Refresh {
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
                    revision: Some(1),
                }),
                completion: super::SessionBackendCompletion::Refresh {
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
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let token = PendingToken::from_raw(42);
        let (completions, receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let result = Ok(SessionCommandResult {
            message: "created".to_owned(),
            sessions: Some(records),
            session_ids: Some(vec![existing, created]),
            agent_resumes: None,
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
        let completion = super::SessionBackendCompletion::Refresh {
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
        let completion = super::SessionBackendCompletion::Refresh {
            before: vec![existing],
            completions,
        };
        super::emit_session_command_result(&Err("daemon unavailable".to_owned()), &completion);
        assert!(matches!(
            receiver.recv().unwrap(),
            AppEvent::Backend(BackendEvent::Notice(notice)) if notice.message == "daemon unavailable"
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[derive(Clone, Copy)]
    enum ConcurrentSessionRequest {
        Create(u64),
        Remove,
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
                SessionCommand::Create { .. } => vec![self.existing, self.created],
                SessionCommand::Remove { .. } => Vec::new(),
                _ => vec![self.existing],
            };
            Ok(SessionCommandResult {
                message: "completed".to_owned(),
                sessions: None,
                session_ids: Some(session_ids),
                agent_resumes: None,
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
                        profile: None,
                        model: None,
                    },
                },
                completions,
            ),
            ConcurrentSessionRequest::Remove => host.remove(
                crate::usecase::application::daemon_backend::RemoveSessionRequest {
                    workspace,
                    session,
                    force: false,
                },
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
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(
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
        drain_controller_host_actions(
            &actions,
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();

        let second_completion = enqueue_session_request(&mut host, second, workspace, session);
        drain_controller_host_actions(
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
                    && result.notice.is_some_and(|notice| {
                        notice.message == "session command is already running"
                    })
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
                revision: None,
            })
        }
    }

    #[test]
    fn session_worker_panic_completes_and_returns_the_port() {
        let snapshot = snapshot("demo");
        let workspace = snapshot.workspace_id;
        let session = snapshot.session_ids[0];
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(
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
        drain_controller_host_actions(
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
        drain_controller_host_actions(
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
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        ui.active_session_command = Some(2);
        let result = Ok(SessionCommandResult::message("done"));
        let (completions, _) = crate::usecase::application::daemon_backend::Completions::channel();

        ui.session_completion_sender
            .send(super::SessionCommandCompletion {
                command_id: 1,
                result: result.clone(),
                completion: super::SessionBackendCompletion::Refresh {
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
                completion: super::SessionBackendCompletion::Refresh {
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
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(
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
        drain_controller_host_actions(
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
        let view = WorkspaceView::with_runtime_ids(
            snapshot.workspace,
            snapshot.state,
            snapshot.session_ids,
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace_id, vec![session]);
        let _ = runtime.apply_event(AppEvent::Resize {
            width: 100,
            height: 30,
        });
        let _ = runtime.apply_event(sidebar_pointer_event(
            5,
            4,
            std::time::Duration::from_millis(1_000),
        ));

        let (completions, receiver) =
            crate::usecase::application::daemon_backend::Completions::channel();
        let result = Ok(SessionCommandResult {
            message: "same snapshot".to_owned(),
            sessions: Some(records),
            session_ids: Some(vec![session]),
            agent_resumes: None,
            revision: None,
        });
        let completion = super::SessionBackendCompletion::Refresh {
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
            4,
            std::time::Duration::from_millis(1_100),
        ));

        assert_eq!(runtime.state().active(), Target::Root(workspace_id));
        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Switch)
        ));
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
            _workspace: WorkspaceId,
            _session: Option<SessionId>,
            _profile: Option<AgentProfileId>,
        ) -> Result<AgentPaneAdmission, String> {
            Ok(AgentPaneAdmission {
                terminal: self.terminal.clone(),
                continuation: None,
            })
        }

        fn attach_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Ok(TerminalAttach {
                subscription: self.subscription,
                connection_epoch: 1,
                output_offset: self.replay.len() as u64,
                replay: self.replay.clone(),
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
            _subscription: u64,
            _input_seq: u64,
            bytes: &[u8],
        ) -> Result<TerminalInputOutcome, TerminalError> {
            if bytes == b"fail" {
                Err(TerminalError::Unavailable)
            } else {
                Ok(TerminalInputOutcome::Written)
            }
        }

        fn detach_terminal(&mut self, _terminal: &TerminalRef, subscription: u64) {
            self.detaches.lock().unwrap().push(subscription);
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

    /// Build a `WorkspaceUi` + `WorkspaceRuntime` with `port` as the daemon
    /// transport, driven into Closeup with a focused live tab attached to
    /// `terminal`. Mirrors the shell's launch → complete → focus → attach path.
    fn focused_live_pane(
        workspace: WorkspaceId,
        session: SessionId,
        terminal: TerminalRef,
        port: Box<dyn AgentCommandPort>,
    ) -> (WorkspaceUi, WorkspaceRuntime) {
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, vec![session], port);
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        // Down selects the session row; Enter activates it into Closeup.
        let _ = runtime.handle_key(Key::Down);
        let _ = runtime.handle_key(Key::Enter);
        let operation = OperationId::new();
        let _ = runtime.request_pane(Target::Session(session), operation, PaneKind::Agent);
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
    fn close_tab_live_action_detaches_the_focused_terminal() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let detaches = Arc::new(Mutex::new(Vec::new()));
        let (ui, mut runtime) = focused_live_pane(
            workspace,
            session,
            terminal.clone(),
            Box::new(ScriptedAgentPort {
                terminal,
                subscription: 8,
                replay: Vec::new(),
                poll_error: None,
                detaches: Arc::clone(&detaches),
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

        assert!(runtime.active_pane().tabs().is_empty());
        assert_eq!(*detaches.lock().unwrap(), vec![8]);
    }

    #[test]
    fn close_tab_live_action_cancels_the_focused_pending_launch() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), vec![session]);
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Down);
        let _ = runtime.handle_key(Key::Enter);
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

        assert!(runtime.active_pane().tabs().is_empty());
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
            _geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Ok(TerminalAttach {
                subscription: 1,
                connection_epoch: 1,
                output_offset: 0,
                replay: Vec::new(),
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
            _subscription: u64,
            _input_seq: u64,
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

        #[allow(clippy::too_many_lines)] // The fake mirrors the production CAS/causal-close matrix.
        fn mutate(
            &mut self,
            workspace: WorkspaceId,
            expected_revision: u64,
            mutation: AgentTabIntentMutation,
        ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
            let mut state = self.state.lock().unwrap();
            assert_eq!(workspace, state.workspace_id);
            let conflict = expected_revision != state.revision;
            self.mutations.lock().unwrap().push(mutation.clone());
            let before = state.clone();
            let force_close_fence = match &mutation {
                AgentTabIntentMutation::Dismiss { continuation }
                | AgentTabIntentMutation::DismissAndSelect { continuation, .. } => {
                    state.targets.iter().any(|target| {
                        target
                            .tabs
                            .iter()
                            .any(|slot| slot.continuation == *continuation)
                    })
                }
                _ => false,
            };
            let mut mutation_applied = true;
            let projection = if conflict {
                match mutation {
                    AgentTabIntentMutation::Observe {
                        terminals,
                        agents,
                        allowed_sessions,
                    } => {
                        mutation_applied = false;
                        Some(state.projected_exact(&terminals, &agents, &allowed_sessions))
                    }
                    AgentTabIntentMutation::Reopen { continuation } => {
                        mutation_applied = !state.dismissed.contains(&continuation);
                        None
                    }
                    AgentTabIntentMutation::Upsert {
                        session_id,
                        continuation,
                        terminal,
                        select,
                    } => {
                        mutation_applied = state.targets.iter().any(|target| {
                            target.session_id == session_id
                                && target.tabs.iter().any(|slot| {
                                    slot.continuation == continuation
                                        && slot.terminal.fences(&terminal)
                                })
                                && (!select || target.selected == Some(continuation))
                                && !state.dismissed.contains(&continuation)
                        });
                        None
                    }
                    AgentTabIntentMutation::DismissAndSelect { continuation, .. }
                    | AgentTabIntentMutation::Dismiss { continuation } => {
                        state.apply(AgentTabIntentMutation::Dismiss { continuation })
                    }
                    AgentTabIntentMutation::Select {
                        session_id,
                        continuation,
                    } => {
                        mutation_applied = state.targets.iter().any(|target| {
                            target.session_id == session_id && target.selected == continuation
                        });
                        None
                    }
                    AgentTabIntentMutation::Reorder {
                        session_id,
                        continuations,
                    } => {
                        mutation_applied = state
                            .targets
                            .iter()
                            .find(|target| target.session_id == session_id)
                            .is_some_and(|target| {
                                target
                                    .tabs
                                    .iter()
                                    .map(|slot| slot.continuation)
                                    .eq(continuations)
                            });
                        None
                    }
                }
            } else {
                match mutation {
                    AgentTabIntentMutation::Upsert {
                        session_id,
                        continuation,
                        terminal,
                        select: _,
                    } if state.dismissed.contains(&continuation) => {
                        mutation_applied = false;
                        state.apply(AgentTabIntentMutation::Upsert {
                            session_id,
                            continuation,
                            terminal,
                            select: false,
                        })
                    }
                    mutation => state.apply(mutation),
                }
            };
            if *state != before || force_close_fence {
                state.revision += 1;
            }
            Ok(AgentTabIntentPortCommit {
                intent: state.clone(),
                projection,
                mutation_applied,
                cas_conflict: conflict,
            })
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

        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
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

        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
                runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), None)
                    .unwrap(),
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
        assert!(retry.reconnect_pending);
        assert!(!retry.complete(now, super::RestoreJobOutcome::Applied));
        assert!(!retry.reconnect_pending);
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
            runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), foreign, Some(added_session))
                .unwrap(),
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

        let view = WorkspaceView::with_runtime_ids(
            ws("demo"),
            state("demo"),
            vec![original_session, added_session],
        );
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
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
                .is_none()
        );
        assert_ne!(runtime.focused_terminal(), Some(terminal));
    }

    #[test]
    fn restore_intent_publish_failure_keeps_bytes_but_does_not_block_generic_restore() {
        let workspace = WorkspaceId::new();
        let generic = scoped_terminal_ref(workspace, None);
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::new(),
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
            &BTreeSet::new(),
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
        let continuation = AgentContinuationRef::new();
        let inventory_only_continuation = AgentContinuationRef::new();
        let agent = scoped_terminal_ref(workspace, None);
        let inventory_only_agent = scoped_terminal_ref(workspace, None);
        let existing_generic = scoped_terminal_ref(workspace, None);
        let new_generic = scoped_terminal_ref(workspace, None);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation,
            terminal: agent.clone(),
            select: true,
        });
        intent.revision = 3;
        let durable = Arc::new(Mutex::new(intent));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
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
                        terminal: agent.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: existing_generic.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(agent.clone()),
            }],
        ));
        let fence = runtime.restore_fence();
        let applied = super::apply_restore_completion(
            super::RestoreCompletion {
                port: Box::new(UnavailableAgentCommandPort),
                dispatched_interaction: fence.0,
                dispatched_registry_revision: fence.1,
                dispatched_allowed_sessions: BTreeSet::new(),
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
                                None,
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
                                None,
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
            &BTreeSet::new(),
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
                notices +=
                    u32::from(retry.complete(now, super::RestoreJobOutcome::TransportFailed));
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
        assert!(matches!(
            runtime.active_pane().tabs(),
            [PaneTab::Live(LivePane { terminal, kind: PaneKind::Agent })]
                if terminal.fences(&second_terminal)
        ));
        assert!(!retry.begin_if_due(redispatch_at + std::time::Duration::from_secs(60)));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The stale and fresh observations must share one durable fixture.
    fn cross_tui_stale_observe_omits_old_ref_then_fresh_observation_restores_replacement() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let old = scoped_terminal_ref(workspace, None);
        let replacement = scoped_terminal_ref(workspace, None);
        let mut initial = AgentTabIntent::empty(workspace);
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation,
            terminal: old.clone(),
            select: true,
        });
        initial.revision = 1;
        let durable = Arc::new(Mutex::new(initial));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let dispatched = runtime.restore_fence();

        // Another TUI replaces O with R after this controller loaded revision 1.
        {
            let mut latest = durable.lock().unwrap();
            latest.apply(AgentTabIntentMutation::Upsert {
                session_id: None,
                continuation,
                terminal: replacement.clone(),
                select: true,
            });
            latest.revision += 1;
        }
        let inventory = |terminal: &TerminalRef| AgentInventory {
            workspace_id: workspace,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), None)
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
                dispatched_allowed_sessions: BTreeSet::new(),
                terminals: Ok(terminals(&old)),
                agents: Ok(inventory(&old)),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::new(),
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
                dispatched_allowed_sessions: BTreeSet::new(),
                terminals: Ok(terminals(&replacement)),
                agents: Ok(inventory(&replacement)),
                observation_coherent: true,
            },
            &mut ui,
            &mut runtime,
            workspace,
            &BTreeSet::new(),
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
        let continuation = AgentContinuationRef::new();
        let old = scoped_terminal_ref(workspace, None);
        let replacement = scoped_terminal_ref(workspace, None);
        let mut initial = AgentTabIntent::empty(workspace);
        initial.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation,
            terminal: old.clone(),
            select: true,
        });
        initial.revision = 1;
        let durable = Arc::new(Mutex::new(initial));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::new(Mutex::new(Vec::new())),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let inventory = |terminal: &TerminalRef| AgentInventory {
            workspace_id: workspace,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), None)
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
                    dispatched_allowed_sessions: BTreeSet::new(),
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
            &BTreeSet::new(),
        );
        assert_eq!(first.outcome, super::RestoreJobOutcome::Applied);
        assert_eq!(runtime.focused_terminal(), Some(old.clone()));

        // Another TUI advances this continuation from O to R. The late O
        // observation updates local durable state but must leave the visible O
        // pane untouched until its immediately scheduled fresh observation.
        {
            let mut latest = durable.lock().unwrap();
            latest.apply(AgentTabIntentMutation::Upsert {
                session_id: None,
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
            &BTreeSet::new(),
        );
        assert_eq!(stale.outcome, super::RestoreJobOutcome::FenceRejected);
        assert_eq!(runtime.focused_terminal(), Some(old.clone()));
        assert_eq!(ui.agent_continuation_for(&old), Some(continuation));

        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert!(runtime.active_pane().tabs().is_empty());
        assert!(durable.lock().unwrap().dismissed.contains(&continuation));
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
            &BTreeSet::new(),
        );
        assert_eq!(fresh.outcome, super::RestoreJobOutcome::Applied);
        assert!(runtime.active_pane().tabs().is_empty());
        assert_ne!(runtime.focused_terminal(), Some(replacement));
    }

    #[test]
    fn successful_restore_retains_port_and_reconnect_reobserves_exactly_once() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let terminal = scoped_terminal_ref(workspace, None);
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
                runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), None)
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
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut retry = super::RestoreRetryState::new();

        assert!(retry.begin_if_due(std::time::Duration::ZERO));
        let fence = runtime.restore_fence();
        super::spawn_restore_job(
            port,
            workspace,
            BTreeSet::new(),
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
            &BTreeSet::new(),
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
            BTreeSet::new(),
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
            &BTreeSet::new(),
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
        let removed_session = SessionId::new();
        let root_open = AgentContinuationRef::new();
        let root_dismissed = AgentContinuationRef::new();
        let removed_selected = AgentContinuationRef::new();
        let removed_dismissed = AgentContinuationRef::new();
        let root_open_terminal = scoped_terminal_ref(workspace, None);
        let root_dismissed_terminal = scoped_terminal_ref(workspace, None);
        let removed_selected_terminal = scoped_terminal_ref(workspace, Some(removed_session));
        let removed_dismissed_terminal = scoped_terminal_ref(workspace, Some(removed_session));
        let mut initial = AgentTabIntent::empty(workspace);
        for (session_id, continuation, terminal, select) in [
            (None, root_open, root_open_terminal.clone(), true),
            (None, root_dismissed, root_dismissed_terminal.clone(), false),
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
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::from([removed_session]),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::clone(&mutations),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
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
                dispatched_allowed_sessions: BTreeSet::from([removed_session]),
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
            &BTreeSet::from([removed_session]),
        );
        assert_eq!(initial_restore.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(std::time::Duration::ZERO, initial_restore.outcome));
        assert_eq!(mutations.lock().unwrap().len(), 1);
        assert!(!ui.take_agent_observation_request());

        ui.set_allowed_agent_sessions(BTreeSet::new());
        assert!(ui.take_agent_observation_request());
        ui.set_allowed_agent_sessions(BTreeSet::new());
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
                dispatched_allowed_sessions: BTreeSet::new(),
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
                            None,
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
            &BTreeSet::new(),
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
            ] if *initial_allowed == BTreeSet::from([removed_session])
                && removed_allowed.is_empty()
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
        );
        assert_eq!(targets.len(), 2);
        let root = targets
            .iter()
            .find(|target| target.target == Target::Root(workspace))
            .unwrap();
        assert_eq!(root.selected, Some(first_terminal));
        assert_eq!(root.panes[0].terminal, second_terminal);
        assert_eq!(root.panes[1].kind, PaneKind::Agent);
        assert_eq!(root.panes[2].terminal, root_generic);
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
        let _ = runtime.handle_key(Key::Down);
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
            }],
        );
        let geometry = terminal_geometry(20, 80);

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
    #[allow(clippy::too_many_lines)] // Close and reopen rollback use the same failure fixture.
    fn persistence_failures_leave_close_and_reopen_ui_unchanged_with_typed_notice() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let terminal = scoped_terminal_ref(workspace, None);
        let mut open_intent = AgentTabIntent::empty(workspace);
        open_intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation,
            terminal: terminal.clone(),
            select: true,
        });
        let durable = Arc::new(Mutex::new(open_intent));
        let attempts = Arc::new(AtomicUsize::new(0));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
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
            }],
        ));

        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert_eq!(runtime.focused_terminal(), Some(terminal));
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

        let mut closed_intent = durable.lock().unwrap().clone();
        closed_intent.apply(AgentTabIntentMutation::Dismiss { continuation });
        let closed = Arc::new(Mutex::new(closed_intent));
        let closed_bytes = serde_json::to_vec(&*closed.lock().unwrap()).unwrap();
        let reopen_attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&closed),
                    error: AgentTabIntentError::ReadOnlySchema,
                    attempts: Arc::clone(&reopen_attempts),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::ReopenAgent(ReopenAgentRequest {
                workspace,
                continuation,
            }))
            .unwrap();
        super::drain_controller_host_actions(
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
        let continuation = AgentContinuationRef::new();
        let agent_terminal = scoped_terminal_ref(workspace, None);
        let generic_terminal = scoped_terminal_ref(workspace, None);
        let durable = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let mutations = Arc::new(Mutex::new(Vec::new()));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(workspace, Vec::new(), Box::new(UnavailableAgentCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
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
            session_id: None,
            continuation,
            terminal: agent_terminal.clone(),
            select: true,
        })
        .unwrap();
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Root(workspace),
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
            }],
        ));
        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );
        assert_eq!(runtime.focused_terminal(), Some(generic_terminal.clone()));
        assert!(durable.lock().unwrap().dismissed.contains(&continuation));

        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(ControllerHostAction::ReopenAgent(ReopenAgentRequest {
                workspace,
                continuation,
            }))
            .unwrap();
        super::drain_controller_host_actions(
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
                dispatched_allowed_sessions: BTreeSet::new(),
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
                            None,
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
            &BTreeSet::new(),
        );
        assert_eq!(applied.outcome, super::RestoreJobOutcome::Applied);
        assert!(!retry.complete(now, applied.outcome));
        let restored = runtime
            .active_pane()
            .tabs()
            .iter()
            .filter_map(|tab| match tab {
                PaneTab::Live(pane) => Some(pane.terminal.clone()),
                PaneTab::Pending(_) | PaneTab::Ready(_) => None,
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
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
                port: Box::new(UnavailableAgentCommandPort),
                outcome: super::PaneLaunchOutcome::Agent {
                    operation,
                    result: Ok(AgentPaneAdmission {
                        terminal: replacement.clone(),
                        continuation: Some(continuation),
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
    fn closing_selected_agent_persists_the_generic_successor_without_focus_drift() {
        let workspace = WorkspaceId::new();
        let first = AgentContinuationRef::new();
        let closed = AgentContinuationRef::new();
        let first_terminal = scoped_terminal_ref(workspace, None);
        let closed_terminal = scoped_terminal_ref(workspace, None);
        let generic = scoped_terminal_ref(workspace, None);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation: first,
            terminal: first_terminal.clone(),
            select: false,
        });
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: None,
            continuation: closed,
            terminal: closed_terminal.clone(),
            select: true,
        });
        let durable = Arc::new(Mutex::new(intent));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(MemoryIntentPort {
                    state: Arc::clone(&durable),
                    mutations: Arc::new(Mutex::new(Vec::new())),
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
                        terminal: first_terminal,
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: closed_terminal,
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: generic.clone(),
                        kind: PaneKind::Terminal,
                    },
                ],
                selected: Some(generic.clone()),
            }],
        ));
        let _ = runtime.focus_terminal(
            Target::Root(workspace),
            durable.lock().unwrap().targets[0].tabs[1].terminal.clone(),
        );

        super::close_focused_terminal_pane(
            &mut ui,
            &mut runtime,
            &mut std::collections::HashMap::new(),
        );

        assert_eq!(runtime.focused_terminal(), Some(generic));
        let durable = durable.lock().unwrap();
        assert!(durable.dismissed.contains(&closed));
        assert_eq!(durable.targets[0].selected, None);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Agent and generic routing share one persistence-failure fixture.
    fn persistence_failures_block_agent_reorder_and_selection_but_not_generic_tabs() {
        let workspace = WorkspaceId::new();
        let first_terminal = scoped_terminal_ref(workspace, None);
        let second_terminal = scoped_terminal_ref(workspace, None);
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let mut intent = AgentTabIntent::empty(workspace);
        for (continuation, terminal, select) in [
            (first, first_terminal.clone(), true),
            (second, second_terminal.clone(), false),
        ] {
            intent.apply(AgentTabIntentMutation::Upsert {
                session_id: None,
                continuation,
                terminal,
                select,
            });
        }
        let durable = Arc::new(Mutex::new(intent));
        let bytes_before = serde_json::to_vec(&*durable.lock().unwrap()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(FailingIntentPort {
                    state: Arc::clone(&durable),
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&attempts),
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
                        terminal: first_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: second_terminal.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(first_terminal.clone()),
            }],
        ));
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
        super::drain_controller_host_actions(
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
        let generic_first = scoped_terminal_ref(workspace, None);
        let generic_second = scoped_terminal_ref(workspace, None);
        let empty = Arc::new(Mutex::new(AgentTabIntent::empty(workspace)));
        let generic_attempts = Arc::new(AtomicUsize::new(0));
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut generic_ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_tab_intent(
                workspace,
                BTreeSet::new(),
                Box::new(FailingIntentPort {
                    state: empty,
                    error: AgentTabIntentError::Unavailable,
                    attempts: Arc::clone(&generic_attempts),
                }),
            );
        let mut generic_runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let fence = generic_runtime.restore_fence();
        assert!(generic_runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![super::PaneRestoreTarget {
                target: Target::Root(workspace),
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
            }],
        ));
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
        super::drain_controller_host_actions(
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
            }],
        );
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Down));
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
        let root_terminal = scoped_terminal_ref(workspace, None);
        let root_agent = scoped_terminal_ref(workspace, None);
        let session_terminal = scoped_terminal_ref(workspace, Some(session));
        let dead = scoped_terminal_ref(workspace, None);
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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

        // The active (root) pane shows exactly the two root runtimes, deduped.
        assert_eq!(runtime.active_pane().tabs().len(), 2);
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
        let terminal = scoped_terminal_ref(workspace, None);
        let agent = scoped_terminal_ref(workspace, None);
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
        let view = WorkspaceView::with_runtime_ids(ws("demo"), state("demo"), Vec::new());
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
            .with_agent_context(
                workspace,
                Vec::new(),
                Box::new(RestoreInventoryPort {
                    entries,
                    fail: false,
                    inputs: inputs.clone(),
                }),
            );
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort))
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
        let mut ui = WorkspaceUi::new(view, Box::new(UnavailableSessionCommandPort));
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
        assert!(begin_terminal_selection_on_click(
            &ui,
            &runtime,
            &mut controls,
            20,
            80,
            rows_len,
            0,
            (37, 5),
        ));
        assert!(controls.has_selection());
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
        assert!(!begin_terminal_selection_on_click(
            &ui,
            &inactive,
            &mut controls,
            20,
            80,
            1,
            0,
            (40, 5),
        ));
        assert!(!begin_terminal_selection_on_click(
            &ui,
            &runtime,
            &mut controls,
            20,
            80,
            1,
            0,
            (0, 0),
        ));
        let empty_view = WorkspaceView::with_runtime_ids(ws("empty"), state("empty"), vec![]);
        let empty_ui = WorkspaceUi::new(empty_view, Box::new(UnavailableSessionCommandPort));
        assert!(!begin_terminal_selection_on_click(
            &empty_ui,
            &runtime,
            &mut controls,
            20,
            80,
            1,
            0,
            (40, 5),
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
    fn a_plain_click_on_a_terminal_link_opens_it_without_touching_the_pty() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let terminal = live_terminal_ref(workspace, session);
        let (ui, runtime) = focused_live_pane(
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
        controls.sync_focus(Some(&terminal));

        // A press-release with no drag: the URL starts at content column 4, so
        // frame column 37 + 4 = 41 lands on it. The click opens the whole link.
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
        assert_eq!(browser.opened, vec!["https://example.com/x".to_owned()]);
        // A pointer release is not keyboard input, so nothing was forwarded to the
        // child PTY, and the clipboard was left alone.
        assert!(term.copied.is_empty());

        // A click on the leading prose (frame column 37 = content column 0) opens
        // nothing.
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
                column: 37,
                row: 5,
            },
        );
        assert_eq!(browser.opened.len(), 1);
    }

    #[test]
    fn a_terminal_press_anchors_a_drag_at_its_start_cell() {
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
        assert!(begin_terminal_selection_on_click(
            &ui,
            &runtime,
            &mut controls,
            20,
            80,
            rows_len,
            0,
            (37, 5),
        ));
        assert!(controls.is_dragging());
        assert_eq!(
            controls.selection().expect("selection started").anchor(),
            TerminalPoint { row: 0, column: 0 }
        );

        // A left-sidebar click remains with sidebar navigation; the terminal
        // interceptor must not consume it.
        assert!(!begin_terminal_selection_on_click(
            &ui,
            &runtime,
            &mut controls,
            20,
            80,
            rows_len,
            0,
            (5, 2),
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
        let (ui, _runtime) = focused_live_pane(
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
        let rows = ui
            .terminal_rows(&terminal, Some(&selection))
            .expect("selection rows");
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
        assert_eq!(
            controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
                .expect("live view")
                .scroll,
            0
        );
        controls.scroll_up();
        controls.scroll_up();
        assert_eq!(
            controller_terminal_view(&ui, &runtime, &mut controls, viewport_rows)
                .expect("live view")
                .scroll,
            2
        );
    }

    /// テスト用 Terminal。キー列を順に返し、描いたフレームを記録する。
    #[derive(Default)]
    struct FakeTerminal {
        keys: VecDeque<Key>,
        frames: Vec<Vec<String>>,
        waits: Vec<std::time::Duration>,
        copied: Vec<String>,
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
            Ok((0, 0))
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
    fn no_metrics_factory_creates_an_empty_port() {
        assert_eq!(NoMetricsFactory.create().latest(), None);
    }

    #[test]
    fn idle_agent_port_is_safe_when_an_unexpected_launch_is_requested() {
        let mut port = IdleAgentPort;
        let error = port
            .launch(WorkspaceId::new(), Some(SessionId::new()), None)
            .unwrap_err();

        assert_eq!(error, "not launched in this test");
        assert_eq!(
            port.launch_terminal(
                WorkspaceId::new(),
                Some(SessionId::new()),
                Geometry { cols: 80, rows: 24 },
                "open",
            )
            .unwrap_err(),
            "terminal launch is unavailable"
        );
    }

    #[derive(Default)]
    struct FakeLoader {
        opened: Vec<PathBuf>,
        cleanup_removed: Vec<PathBuf>,
        cleanup_calls: usize,
        unregistered: Vec<PathBuf>,
        unregister_calls: usize,
        created: Vec<NewRequest>,
        fail: bool,
        /// Number of leading `create_workspace` calls that reject before the
        /// loader starts succeeding, standing in for a pre-flight rejection
        /// (e.g. the workspace already exists) that the user then corrects.
        create_failures: usize,
        opened_at: Option<DateTime<Utc>>,
    }

    impl WorkspaceLoader for FakeLoader {
        fn open(&mut self, path: &Path) -> io::Result<WorkspaceSnapshot> {
            self.opened.push(path.to_path_buf());
            if self.fail {
                return Err(io::Error::other("open failed"));
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace");
            let mut snapshot = snapshot(name);
            if let Some(opened_at) = self.opened_at {
                snapshot.workspace.updated_at = opened_at;
            }
            Ok(snapshot)
        }

        fn cleanup_missing(&mut self, _workspaces: &[Workspace]) -> io::Result<Vec<PathBuf>> {
            self.cleanup_calls += 1;
            Ok(self.cleanup_removed.clone())
        }

        fn unregister(&mut self, paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
            self.unregister_calls += 1;
            self.unregistered.extend_from_slice(paths);
            Ok(paths.to_vec())
        }

        fn create_workspace(&mut self, request: &NewRequest) -> io::Result<WorkspaceSnapshot> {
            self.created.push(request.clone());
            if self.create_failures > 0 {
                self.create_failures -= 1;
                // Mirror the real loader's pre-flight rejection: no workspace is
                // created, so the caller keeps the draft and can retry.
                return Err(io::Error::other(
                    "this directory is already a registered workspace",
                ));
            }
            // Both modes resolve to a directory that is then opened like any
            // other workspace, mirroring the real loader.
            let path = match request {
                NewRequest::Clone { destination, .. } => destination.clone(),
                NewRequest::Existing { path, .. } => path.clone(),
            };
            self.open(&path)
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
    fn startup_splash_draws_and_paces_every_v1_frame_without_reading_input() {
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
        assert_eq!(term.frames.len(), keys.len() - 1);
        assert!(
            term.frames
                .iter()
                .all(|frame| frame.join("\n").contains("Menu"))
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
        assert_eq!(direct.frames.len(), 2);
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
        for key in [Key::Up, Key::Down, Key::Left, Key::Right, Key::CtrlQ] {
            let _ = step_config(&mut config, key, &mut settings);
        }
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
        fail_save: bool,
    }

    #[derive(Default)]
    struct WorkspaceBindingSettingsPort {
        selected: Vec<PathBuf>,
        saves: Vec<(SettingsScope, Settings)>,
    }

    impl SettingsPort for WorkspaceBindingSettingsPort {
        fn select_workspace(&mut self, workspace_root: &Path) -> io::Result<()> {
            self.selected.push(workspace_root.to_path_buf());
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
    }

    // Focus the dirty Save row from Global Config: cycle the theme, then step down to
    // Save (Theme → Modal mode → Agent model → Issue → Memory → Save).
    const CONFIG_SAVE_KEYS: [Key; 7] = [
        Key::Right,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
    ];

    // Workspace Config starts on Agent and contains only Agent → Issue →
    // Memory → Save.
    const WORKSPACE_CONFIG_SAVE_KEYS: [Key; 5] =
        [Key::Right, Key::Down, Key::Down, Key::Down, Key::Enter];

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
        run_workspace_config(&mut back, &mut settings, AvailableAgentModels::all(), &base).unwrap();

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
    fn workspace_config_swallows_quit_keys_until_escape() {
        let base = vec!["home".to_owned(); 24];
        let mut settings = RecordingSettingsPort::default();
        let mut term =
            FakeTerminal::with_keys(&[Key::Quit, Key::CtrlQ, Key::Char('q'), Key::Escape]);

        run_workspace_config(&mut term, &mut settings, AvailableAgentModels::all(), &base).unwrap();

        assert_eq!(term.frames.len(), 4);
        assert!(
            term.frames
                .iter()
                .all(|frame| frame.join("\n").contains("Config"))
        );
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
        assert_eq!(unite_loader.opened, vec![PathBuf::from("/tmp/alpha")]);
    }

    #[test]
    fn open_unregister_requires_confirmation_and_only_passes_the_selected_path_to_loader() {
        let alpha = ws("alpha");
        let beta = ws("beta");

        let mut cancel = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Down,
            Key::CtrlD,
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
            Key::CtrlD,
            Key::Enter,
            Key::Quit,
        ]);
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
                .contains("No tabs stirring yet. Enter starts one.")
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
        assert_eq!(term.frames.len(), keys.len());

        let keys = [Key::Char('o'), Key::Tab, Key::Enter, Key::Quit];
        let mut term = FakeTerminal::with_keys(&keys);
        run(
            &mut term,
            vec![ws("alpha")],
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert_eq!(term.frames.len(), keys.len());
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

        let _ = step_open(&mut open, Key::CtrlD);
        let _ = step_open(&mut open, Key::Left);
        assert!(matches!(step_open(&mut open, Key::Escape), OpenStep::Stay));
        let _ = step_open(&mut open, Key::CtrlD);
        assert!(matches!(
            step_open(&mut open, Key::Char('y')),
            OpenStep::ConfirmUnregister(_)
        ));

        for key in [Key::Right, Key::Tab, Key::Char('n'), Key::CtrlQ] {
            let mut open = Open::new(vec![ws("fresh")]);
            let _ = step_open(&mut open, Key::CtrlD);
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
    fn unite_recent_stays_without_loading_a_workspace() {
        let unite = Recent::Unite(UniteOverview::new(vec![
            WorkspaceOverview::new(ws("primary"), 0, 0, 0),
            WorkspaceOverview::new(ws("other"), 0, 0, 0),
        ]));
        let empty = Recent::Unite(UniteOverview::new(Vec::new()));
        let keys = [Key::Char('2'), Key::Char('1'), Key::Char('q'), Key::Enter];
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
        assert!(loader.opened.is_empty());
        assert_eq!(term.frames.len(), 3);
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
        assert_eq!(term.frames.len(), 2);
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
            port.launch(WorkspaceId::new(), Some(SessionId::new()), None)
                .is_err()
        );
        assert_eq!(
            port.resize_terminal(&terminal, Geometry { cols: 80, rows: 24 }),
            Ok(())
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
            port.input_terminal(&terminal, 1, 0, b"x"),
            Err(TerminalError::Unavailable)
        );
        // Detach is a no-op default and must not panic.
        port.detach_terminal(&terminal, 1);
        assert_eq!(
            port.launch_terminal(
                WorkspaceId::new(),
                Some(SessionId::new()),
                Geometry { cols: 80, rows: 24 },
                "open",
            ),
            Err("terminal launch is unavailable".to_owned())
        );
        // The default discovers no runtimes, so an embedder without a daemon
        // simply opens a workspace with no restored panes.
        assert_eq!(port.list_terminals(), Ok(Vec::new()));
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
        // A paste is wrapped in bracketed-paste markers so the agent inserts the
        // multi-line text as one block; an empty paste sends nothing.
        assert_eq!(
            key_to_terminal_bytes(Key::Paste("a\nb".to_owned())),
            Some(b"\x1b[200~a\nb\x1b[201~".to_vec())
        );
        assert_eq!(key_to_terminal_bytes(Key::Paste(String::new())), None);
        assert_eq!(key_to_terminal_bytes(Key::Quit), Some(vec![3]));
        assert_eq!(key_to_terminal_bytes(Key::CtrlQ), Some(vec![17]));
        assert_eq!(key_to_terminal_bytes(Key::CtrlD), Some(vec![4]));
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
        // The non-interactive `usagi launch <path>` fallback renders one static
        // Home frame through the controller projection: the workspace name, its
        // sessions, and the `+ new session` row.
        let frame = render_home_snapshot(30, 100, &snapshot("demo")).join("\n");
        assert!(frame.contains("demo"));
        assert!(frame.contains("demo-session"));
        assert!(frame.contains("+ new session"));
        // A zero size safely falls back to the default geometry.
        assert!(!render_home_snapshot(0, 0, &snapshot("demo")).is_empty());
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
    fn banner_screen_runner_names_every_tui_screen() {
        let entries = [
            EntryScreen::Welcome,
            EntryScreen::Workspace {
                path: PathBuf::from("/tmp/project"),
            },
            EntryScreen::Config,
            EntryScreen::Doctor,
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
             usagi v0.1.0: config TUI\n\
             usagi v0.1.0: doctor TUI\n"
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
}

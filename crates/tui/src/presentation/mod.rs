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
pub mod theme;
pub mod views;
pub mod widgets;
pub mod workspace_runtime;

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use chrono::{DateTime, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId};
use usagi_core::domain::recent::Recent;
use usagi_core::domain::workspace::Workspace;
use usagi_core::usecase::client::DaemonMetrics;

use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::views::config::{self, AvailableAgentModels, Config};
use crate::presentation::views::new::{self, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::pr_modal::PrModal;
use crate::presentation::views::splash;
use crate::presentation::views::welcome::{self, MenuAction, Welcome};
use crate::presentation::views::workspace::{
    self, GitDiff, HomeProjection, ProjectedSession, TerminalViewProjection,
    Workspace as WorkspaceView, render_home,
};
use crate::presentation::views::{create_session_modal, quit_modal};
use crate::presentation::widgets::modal::{self, ConfirmationView};
use crate::presentation::workspace_runtime::WorkspaceRuntime;
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, BackendEvent, Effect, Notice, Overlay, SafeError, SafeMessage,
    Target,
};
use crate::usecase::application::pane_runtime::Geometry;
use crate::usecase::application::pr::{BrowserOpener, PrSnapshotPort, canonical_browser_url};
use crate::usecase::application::terminal_session::{
    TerminalAttach, TerminalChunk, TerminalError, TerminalSession, TerminalStreamPort,
};
use crate::usecase::application::{Key, ScreenRunner, Terminal};
use crate::usecase::overview::SessionCommand;
use crate::usecase::terminal_input::LiveTerminalAction;
use usagi_core::usecase::settings::SettingsPort;

pub use crate::usecase::application::{WorkspaceLoader, WorkspaceSnapshot};

/// Daemon-authoritative Agent launch boundary for the workspace runtime.
pub trait AgentCommandPort: Send {
    /// # Errors
    ///
    /// Returns a presentation-safe daemon launch failure.
    fn launch(
        &mut self,
        workspace: WorkspaceId,
        session: SessionId,
        profile: Option<AgentProfileId>,
    ) -> Result<TerminalRef, String>;

    /// Open a daemon-owned login shell for an existing managed session.
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
        _session: SessionId,
        _geometry: Geometry,
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
    ) -> Result<(), TerminalError> {
        Err(TerminalError::Unavailable)
    }

    /// Release a daemon terminal subscription; it must not stop the process.
    fn detach_terminal(&mut self, _terminal: &TerminalRef, _subscription: u64) {}
}

/// Bridges the workspace [`AgentCommandPort`] into the [`TerminalStreamPort`]
/// expected by a [`TerminalSession`], so the session coordinator stays free of
/// the wider Agent launch vocabulary.
struct AgentStreamPort<'a>(&'a mut dyn AgentCommandPort);

impl TerminalStreamPort for AgentStreamPort<'_> {
    #[coverage(off)]
    fn resize(&mut self, terminal: &TerminalRef, geometry: Geometry) -> Result<(), TerminalError> {
        self.0.resize_terminal(terminal, geometry)
    }

    #[coverage(off)]
    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        self.0.attach_terminal(terminal, geometry)
    }
    #[coverage(off)]
    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.0.poll_terminal(terminal, after_offset)
    }
    #[coverage(off)]
    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: u64,
        input_seq: u64,
        bytes: &[u8],
    ) -> Result<(), TerminalError> {
        self.0
            .input_terminal(terminal, subscription, input_seq, bytes)
    }
    #[coverage(off)]
    fn detach(&mut self, terminal: &TerminalRef, subscription: u64) {
        self.0.detach_terminal(terminal, subscription);
    }
}

/// Maps a management [`Key`] to the bytes a focused live terminal should
/// receive. Reserved prefix actions ([`Key::Live`]) do not reach the shell;
/// all other keys, including global controls, do while Closeup owns the pane.
#[coverage(off)]
fn key_to_terminal_bytes(key: Key) -> Option<Vec<u8>> {
    let bytes = match key {
        Key::Passthrough(bytes) => return (!bytes.is_empty()).then(|| bytes.clone()),
        Key::Char(ch) => ch.to_string().into_bytes(),
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::Right => b"\x1b[C".to_vec(),
        Key::Left => b"\x1b[D".to_vec(),
        Key::Quit => vec![3],
        Key::CtrlQ => vec![17],
        Key::CtrlD => vec![4],
        Key::Live(_) | Key::Click { .. } | Key::Pointer(_) | Key::Other => {
            return None;
        }
    };
    Some(bytes)
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

/// 起動バナーを `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
#[coverage(off)]
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
    /// Persisted successfully; show the confirmation frame, then return home.
    Saved,
}

/// New 画面でキー `key` を処理した結果の遷移。
enum NewStep {
    /// 同じ画面に留まる（フォーム編集を続ける）。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceStep {
    Quit,
}

/// Overview の session command を daemon 所有の lifecycle runner へ渡す境界。
///
/// TUI は session store や git worktree を直接操作しない。実行時の合成ルートが
/// daemon IPC client をこの port として注入し、テストは fake port で command と
/// target の対応だけを検証する。
pub trait SessionCommandPort: Send {
    /// Execute one parsed Overview session command for this workspace and its
    /// currently selected session, when the command requires one.
    ///
    /// # Errors
    ///
    /// Returns a safe message when the daemon cannot accept the request.
    #[coverage(off)]
    fn execute(
        &mut self,
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
}

impl SessionCommandResult {
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            sessions: None,
            session_ids: None,
        }
    }
}

struct UnavailableSessionCommandPort;

impl SessionCommandPort for UnavailableSessionCommandPort {
    #[coverage(off)]
    fn execute(
        &mut self,
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
    #[coverage(off)] // Compatibility fallback for embedders without the daemon Agent port.
    fn launch(
        &mut self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _profile: Option<AgentProfileId>,
    ) -> Result<TerminalRef, String> {
        Err("Agent launch is unavailable.".to_owned())
    }
}

/// PR snapshot fallback for entry points that do not inject the daemon PR port
/// (the Welcome/Open/Recent screen graph). The PR overlay shows a safe notice.
struct UnavailablePrSnapshotPort;
impl PrSnapshotPort for UnavailablePrSnapshotPort {
    #[coverage(off)] // Compatibility fallback for embedders without the daemon PR port.
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
    #[coverage(off)] // Compatibility fallback; production injects the composition-root opener.
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
/// session cache ([`WorkspaceView`]), pane launch workers, live terminal
/// streams, and the metrics observation. Home row state, input, and rendering
/// belong to the controller (`AppState`/`render_home`), not here.
struct WorkspaceUi {
    workspace: WorkspaceView,
    /// A create owns the port in its worker until completion, preventing a
    /// second lifecycle request while its sidebar skeleton is visible.
    session_commands: Option<Box<dyn SessionCommandPort>>,
    session_completions: Receiver<SessionCommandCompletion>,
    session_completion_sender: Sender<SessionCommandCompletion>,
    metrics_port: Box<dyn MetricsPort>,
    agent: Option<AgentContext>,
    pane_launches: Vec<PaneLaunch>,
    pane_completions: Receiver<PaneLaunchCompletion>,
    pane_completion_sender: Sender<PaneLaunchCompletion>,
    /// Live coordinators for daemon-owned terminals opened in this workspace,
    /// one per live terminal tab.  Detached/closed tabs are pruned lazily.
    terminals: Vec<TerminalSession>,
    terminal_size: (usize, usize),
}

struct AgentContext {
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    /// A launch worker temporarily owns this port. Terminal streaming resumes
    /// only after the worker returns it with the daemon result.
    port: Option<Box<dyn AgentCommandPort>>,
}

struct SessionCommandCompletion {
    port: Box<dyn SessionCommandPort>,
    result: Result<SessionCommandResult, String>,
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
        result: Result<TerminalRef, String>,
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
        session: SessionId,
        profile: Option<AgentProfileId>,
    },
    Terminal {
        operation: OperationId,
        workspace: WorkspaceId,
        session: SessionId,
    },
}

impl WorkspaceUi {
    #[coverage(off)]
    fn new(workspace: WorkspaceView, session_commands: Box<dyn SessionCommandPort>) -> Self {
        let (session_completion_sender, session_completions) = mpsc::channel();
        let (pane_completion_sender, pane_completions) = mpsc::channel();
        Self {
            workspace,
            session_commands: Some(session_commands),
            session_completions,
            session_completion_sender,
            metrics_port: Box::new(NoMetrics),
            agent: None,
            pane_launches: Vec::new(),
            pane_completions,
            pane_completion_sender,
            terminals: Vec::new(),
            terminal_size: (0, 0),
        }
    }

    fn with_metrics_port(mut self, metrics_port: Box<dyn MetricsPort>) -> Self {
        self.metrics_port = metrics_port;
        self
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

    /// Attach to a freshly launched daemon terminal and start streaming it.
    ///
    /// A failed attach still records the session so its safe feedback renders;
    /// it never spawns a local process.
    #[coverage(off)]
    fn start_terminal_session(&mut self, terminal: TerminalRef, geometry: Geometry) {
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

    #[coverage(off)]
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

    /// Poll the live terminal `terminal` once and return its rendered rows, or
    /// `None` when no attached session matches. The controller runtime picks the
    /// focused terminal, so this takes the ref explicitly instead of reading the
    /// legacy view's focus.
    #[coverage(off)]
    fn poll_terminal_rows(&mut self, terminal: &TerminalRef) -> Option<Vec<String>> {
        let port = self
            .agent
            .as_mut()
            .and_then(|agent| agent.port.as_deref_mut())?;
        let session = self
            .terminals
            .iter_mut()
            .find(|session| session.terminal().fences(terminal))?;
        session.poll(&mut AgentStreamPort(port));
        Some(session.display_rows_with_scrollback())
    }

    /// Forward raw passthrough bytes to the live terminal `terminal`. Returns
    /// `false` when no attached session matches.
    #[coverage(off)]
    fn send_terminal_bytes(&mut self, terminal: &TerminalRef, bytes: &[u8]) -> bool {
        let Some(port) = self
            .agent
            .as_mut()
            .and_then(|agent| agent.port.as_deref_mut())
        else {
            return false;
        };
        let Some(session) = self
            .terminals
            .iter_mut()
            .find(|session| session.terminal().fences(terminal))
        else {
            return false;
        };
        session.send_input(&mut AgentStreamPort(port), bytes);
        true
    }
}

/// welcome のメニュー操作を画面遷移へ写す。
#[coverage(off)]
fn welcome_action(action: MenuAction) -> WelcomeStep {
    match action {
        MenuAction::Quit => WelcomeStep::Quit,
        MenuAction::Open => WelcomeStep::OpenList,
        MenuAction::OpenRecent(index) => WelcomeStep::OpenRecent(index),
        MenuAction::New => WelcomeStep::NewForm,
        MenuAction::Config => WelcomeStep::ConfigScreen,
    }
}

/// Config 画面のキー処理。Save は dirty な Save 行でのみ有効で、成功後は confirmation
/// frame を表示して welcome へ戻る。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_config(config: &mut Config, key: Key, settings: &mut dyn SettingsPort) -> ConfigStep {
    match key {
        Key::Tab => {
            config.toggle_scope();
            ConfigStep::Stay
        }
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
        Key::Enter if config.can_save() => {
            if config.save(settings) {
                ConfigStep::Saved
            } else {
                ConfigStep::Stay
            }
        }
        Key::Escape => ConfigStep::Back,
        Key::Quit | Key::CtrlQ => ConfigStep::Quit,
        _ => ConfigStep::Stay,
    }
}

/// welcome 画面のキー処理。最上位画面なので Esc も終了として扱う。
#[coverage(off)]
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
        | Key::Backspace
        | Key::Tab
        | Key::CtrlD
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => WelcomeStep::Stay,
    }
}

/// New 画面のキー処理（純粋）。矢印キーでフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
#[coverage(off)]
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
        Key::Backspace => {
            form.backspace();
            NewStep::Stay
        }
        Key::Char(ch) => {
            form.insert_char(ch);
            NewStep::Stay
        }
        Key::Escape => NewStep::Back,
        Key::Quit | Key::CtrlQ => NewStep::Quit,
        Key::Tab => {
            form.complete_directory();
            NewStep::Stay
        }
        Key::Enter
        | Key::CtrlD
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => NewStep::Stay,
    }
}

/// New 画面の ←→ 操作。モード選択にフォーカスがあるときはモードを切り替え、テキスト欄では
/// キャレットを左右へ動かす（`right` が右方向）。
#[coverage(off)]
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
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
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
            if paths.is_empty() {
                OpenStep::Stay
            } else {
                OpenStep::Choose(paths)
            }
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
        Key::Live(_) | Key::Click { .. } | Key::Pointer(_) | Key::Passthrough(_) | Key::Other => {
            OpenStep::Stay
        }
    }
}

/// Start a create without blocking the terminal event loop. The sidebar gains a
/// v1-style skeleton immediately; the worker returns the port with its result
/// so later commands still share the same daemon client state.
#[coverage(off)]
fn begin_session_create(ui: &mut WorkspaceUi, command: SessionCommand) {
    let SessionCommand::Create { name } = command else {
        return;
    };
    // A create owns the port until its worker returns it; a second request while
    // one is in flight is a no-op here (the controller create overlay owns the
    // user-facing "already running" feedback).
    let Some(mut port) = ui.session_commands.take() else {
        return;
    };
    let workspace = ui.workspace.record().clone();
    let sender = ui.session_completion_sender.clone();
    std::thread::spawn(move || {
        let result = port.execute(&workspace, None, SessionCommand::Create { name });
        let _ = sender.send(SessionCommandCompletion { port, result });
    });
}

/// Reconcile sidebar rows and the IDs used by Agent/terminal requests as one
/// daemon-authoritative observation.  Legacy/test ports may provide rows only;
/// they retain the existing non-runtime projection behaviour.
#[coverage(off)]
fn apply_session_projection(
    ui: &mut WorkspaceUi,
    sessions: Option<Vec<usagi_core::domain::session::SessionRecord>>,
    session_ids: Option<Vec<SessionId>>,
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
}

/// Receive completed create workers before drawing the next frame. The returned
/// port is reclaimed for the next command and a successful daemon snapshot is
/// reconciled into the session cache, which [`sync_runtime_sessions`] then
/// promotes into the controller's Home rows.
#[coverage(off)]
fn drain_session_completions(ui: &mut WorkspaceUi) {
    while let Ok(completion) = ui.session_completions.try_recv() {
        ui.session_commands = Some(completion.port);
        if let Ok(result) = completion.result {
            apply_session_projection(ui, result.sessions, result.session_ids);
        }
    }
}

/// Start one daemon launch after its pending tab has reached the terminal.
///
/// The port travels with the worker and comes back through
/// [`PaneLaunchCompletion`]. This is deliberately the same ownership pattern
/// as session creation: a slow daemon request never blocks input, wave redraws,
/// or the interaction marker that suppresses automatic focus.
#[coverage(off)]
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
            } => {
                let Some(mut port) = ui.agent.as_mut().and_then(|agent| agent.port.take()) else {
                    ui.pane_launches.push(PaneLaunch::Agent {
                        operation,
                        workspace,
                        session,
                        profile,
                    });
                    continue;
                };
                let sender = ui.pane_completion_sender.clone();
                std::thread::spawn(move || {
                    let result = port.launch(workspace, session, profile);
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
            } => {
                let Some(mut port) = ui.agent.as_mut().and_then(|agent| agent.port.take()) else {
                    ui.pane_launches.push(PaneLaunch::Terminal {
                        operation,
                        workspace,
                        session,
                    });
                    continue;
                };
                let sender = ui.pane_completion_sender.clone();
                std::thread::spawn(move || {
                    let result = port.launch_terminal(workspace, session, geometry);
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
/// Returns `None` for input the Home reducer never consumes: raw PTY passthrough,
/// pointer/click cells (the shell hit-tests those into [`AppKey::SelectRow`] via
/// [`HomeProjection::row_at`]), and keys with no Home management meaning.
///
/// [`HomeProjection::row_at`]: crate::presentation::views::workspace::HomeProjection::row_at
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn app_event_from_key(key: Key) -> Option<AppEvent> {
    let app_key = match key {
        Key::Live(action) => return live_action_to_app_key(action).map(AppEvent::Key),
        Key::Other => return Some(AppEvent::Tick),
        Key::Up => AppKey::Up,
        Key::Down => AppKey::Down,
        Key::Enter => AppKey::Enter,
        Key::Backspace => AppKey::Backspace,
        Key::Tab => AppKey::Tab,
        Key::Escape => AppKey::Escape,
        Key::Char(character) => AppKey::Char(character),
        Key::Quit => AppKey::CtrlC,
        Key::CtrlQ => AppKey::CtrlQ,
        // Input the Home reducer never consumes: raw PTY passthrough and pointer
        // cells (the shell hit-tests pointer/click into `AppKey::SelectRow`),
        // Left/Right (tab motion is Ctrl-N/P), and Ctrl-D (Open Workspace only).
        Key::Passthrough(_)
        | Key::Pointer(_)
        | Key::Click { .. }
        | Key::Left
        | Key::Right
        | Key::CtrlD => return None,
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
        | LiveTerminalAction::ScrollUp
        | LiveTerminalAction::ScrollDown
        | LiveTerminalAction::CopyTerminalSelection => None,
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

#[coverage(off)]
fn render_open(height: usize, width: usize, open: &Open, now: DateTime<Utc>) -> Vec<String> {
    let base = open::render(height, width, open, now);
    let Some(path) = open.unregistering_path() else {
        return base;
    };
    let title = Style::new()
        .fg(Color::White)
        .bold()
        .paint("Unregister workspace");
    let heading = Style::new()
        .fg(Color::White)
        .bold()
        .paint(&format!("Unregister {}?", path.display()));
    modal::render_confirmation_over(
        height,
        width,
        &base,
        open.unregister_confirmation(),
        ConfirmationView {
            title: &title,
            inner_width: 52,
            heading,
            message: "Only the registry entry is removed. Files stay.",
            confirm_role: Role::Danger,
        },
    )
}

/// Recent が指す単体 workspace path。Unite の runtime は今回の対象外なので開かない。
#[coverage(off)]
fn recent_path(recent: &Recent) -> Option<&Path> {
    match recent {
        Recent::Workspace(overview) => Some(&overview.workspace.path),
        Recent::Unite(_) => None,
    }
}

/// Loop control for the controller-driven workspace runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControllerFlow {
    Continue,
    Exit,
}

/// Project the daemon-authoritative session records into the controller's Home
/// row material, in the same order the runtime holds their IDs.
#[coverage(off)]
fn project_controller_sessions(ui: &WorkspaceUi) -> Vec<ProjectedSession> {
    ui.workspace
        .sessions()
        .iter()
        .zip(ui.workspace.session_ids())
        .map(|(record, id)| ProjectedSession::from_record(*id, record))
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
        snapshot.workspace.name.clone(),
        snapshot.workspace.path.clone(),
        &sessions,
    );
    render_home(height, width, &projection)
}

/// Keep the controller's Home rows in step with the daemon session projection
/// the legacy transport reconciled this frame.
#[coverage(off)]
fn sync_runtime_sessions(runtime: &mut WorkspaceRuntime, ui: &WorkspaceUi) {
    let ids = ui.workspace.session_ids().to_vec();
    if runtime.state().sessions() != ids.as_slice() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Sessions(ids)));
    }
}

/// Poll the focused live terminal and project its rows for `with_terminal_view`.
#[coverage(off)]
fn controller_terminal_view(
    ui: &mut WorkspaceUi,
    runtime: &WorkspaceRuntime,
) -> Option<TerminalViewProjection> {
    let terminal = runtime.focused_terminal()?;
    let rows = ui.poll_terminal_rows(&terminal)?;
    Some(TerminalViewProjection {
        rows,
        scroll: 0,
        feedback: None,
    })
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
) -> Vec<String> {
    let projection =
        HomeProjection::from_state(runtime.state(), workspace_name, root_cwd, sessions)
            .with_pane(runtime.active_pane())
            .with_metrics(metrics)
            .with_git_diffs(git_diffs)
            .with_terminal_view(terminal_view);
    let frame = render_home(height, width, &projection);
    // The create form exists exactly when its overlay is open, so keying off it
    // avoids an unreachable "create overlay without a form" branch.
    if let Some(form) = runtime.state().create_session_form() {
        return create_session_modal::render_over(height, width, &frame, form);
    }
    if runtime.state().overlay() == Some(Overlay::QuitConfirmation) {
        return quit_modal::render_over(height, width, &frame);
    }
    frame
}

/// Execute one controller [`Effect`] against the legacy daemon transport. Pane
/// launches record their target so the completion can promote the matching tab.
#[coverage(off)]
fn dispatch_controller_effect(
    ui: &mut WorkspaceUi,
    effect: &Effect,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) -> ControllerFlow {
    match effect {
        Effect::CreateSession { intent, .. } => {
            begin_session_create(
                ui,
                SessionCommand::Create {
                    name: intent.name.clone(),
                },
            );
        }
        Effect::LaunchAgent {
            workspace,
            session,
            operation_id,
            profile,
        } => {
            pending_targets.insert(*operation_id, Target::Session(*session));
            ui.pane_launches.push(PaneLaunch::Agent {
                operation: *operation_id,
                workspace: *workspace,
                session: *session,
                profile: profile.clone(),
            });
        }
        Effect::OpenTerminal {
            target: Target::Session(session),
            operation_id,
            ..
        } => {
            if let Some(agent) = ui.agent.as_ref() {
                let workspace = agent.workspace;
                pending_targets.insert(*operation_id, Target::Session(*session));
                ui.pane_launches.push(PaneLaunch::Terminal {
                    operation: *operation_id,
                    workspace,
                    session: *session,
                });
            }
        }
        Effect::Detach => return ControllerFlow::Exit,
        // RefreshSessions is reconciled every frame; SelectTab is mirrored by
        // `on_effect`; the PR/preview overlay effects are refluxed by
        // `controller_overlay_events` before this executor runs; notes/
        // environment/workspace-command/remove and the entry effects are not
        // reachable from the controller Home input yet.
        Effect::OpenTerminal { .. }
        | Effect::RefreshSessions { .. }
        | Effect::SelectTab { .. }
        | Effect::RemoveSession { .. }
        | Effect::WorkspaceCommand { .. }
        | Effect::LoadNotes { .. }
        | Effect::SaveNotes { .. }
        | Effect::LoadEnvironment { .. }
        | Effect::SaveEnvironment { .. }
        | Effect::LoadPullRequests { .. }
        | Effect::LoadPreview { .. }
        | Effect::OpenPullRequest { .. }
        | Effect::AttachWorkspace { .. }
        | Effect::CloneProject { .. }
        | Effect::RegisterWorkspace { .. }
        | Effect::RefreshDecisions { .. }
        | Effect::ResolveDecision { .. } => {}
    }
    ControllerFlow::Continue
}

/// Execute the Home PR/preview overlay effects against the legacy daemon
/// transport and return the [`AppEvent`]s that reflux their result to the
/// controller. Every other effect yields nothing here; it is handled by
/// [`dispatch_controller_effect`]. This keeps the `effect -> execute -> event ->
/// update()` loop single-directional while the live IO stays in the shell.
#[coverage(off)]
fn controller_overlay_events(
    pr_port: &mut dyn PrSnapshotPort,
    browser: &mut dyn BrowserOpener,
    workspace_name: &str,
    root_cwd: &Path,
    sessions: &[ProjectedSession],
    effect: &Effect,
) -> Vec<AppEvent> {
    match effect {
        Effect::LoadPullRequests { target } => {
            vec![AppEvent::Backend(controller_pull_requests(
                pr_port, *target,
            ))]
        }
        Effect::LoadPreview { target } => vec![AppEvent::Backend(BackendEvent::PreviewLoaded {
            target: *target,
            lines: controller_preview_lines(*target, workspace_name, root_cwd, sessions),
        })],
        Effect::OpenPullRequest { url } => controller_open_pull_request(browser, url)
            .into_iter()
            .map(AppEvent::Backend)
            .collect(),
        _ => Vec::new(),
    }
}

/// Fetch the PR list for `target` and project it into a controller backend event.
/// The workspace root has no PR scope, so it resolves to an empty list.
#[coverage(off)]
fn controller_pull_requests(pr_port: &mut dyn PrSnapshotPort, target: Target) -> BackendEvent {
    let Target::Session(session) = target else {
        return BackendEvent::PullRequestsLoaded {
            target,
            prs: Vec::new(),
        };
    };
    match pr_port.snapshot(session) {
        Ok(snapshot) => BackendEvent::PullRequestsLoaded {
            target,
            prs: PrModal::from_entries(&snapshot.entries).prs().to_vec(),
        },
        Err(message) => BackendEvent::PullRequestsError {
            target,
            error: safe_overlay_error(&message, "pr-load"),
        },
    }
}

/// Build the Markdown preview lines for `target` from the projected sidebar data.
/// This mirrors the pre-controller target summary (label, path, PR count).
#[coverage(off)]
fn controller_preview_lines(
    target: Target,
    workspace_name: &str,
    root_cwd: &Path,
    sessions: &[ProjectedSession],
) -> Vec<String> {
    match target {
        Target::Root(_) => vec![
            format!("workspace: {workspace_name}"),
            format!("path: {}", root_cwd.display()),
        ],
        Target::Session(id) => sessions
            .iter()
            .find(|session| session.id == id)
            .map_or_else(
                || vec![format!("session: {workspace_name}")],
                |session| {
                    let mut lines = vec![
                        format!("session: {}", session.label),
                        format!("path: {}", session.cwd.display()),
                    ];
                    if let Some(summary) = &session.pr_summary {
                        lines.push(summary.clone());
                    }
                    lines
                },
            ),
    }
}

/// Open a selected PR URL in the browser, refluxing a safe notice on failure.
#[coverage(off)]
fn controller_open_pull_request(
    browser: &mut dyn BrowserOpener,
    url: &str,
) -> Option<BackendEvent> {
    let Some(url) = canonical_browser_url(url) else {
        return Some(BackendEvent::Notice(Notice::new(
            "Cannot open an invalid PR URL.",
        )));
    };
    match browser.open(&url) {
        Ok(()) => None,
        Err(message) => Some(BackendEvent::Notice(Notice::new(format!(
            "Could not open browser: {message}"
        )))),
    }
}

/// Wrap a port's already display-safe message as a [`SafeError`] for an overlay.
#[coverage(off)]
fn safe_overlay_error(message: &str, error_id: &str) -> SafeError {
    SafeError {
        message: SafeMessage::new(message),
        error_id: error_id.to_owned(),
    }
}

/// Apply completed pane launches: promote and focus the runtime tab, then attach
/// the daemon terminal stream, so the live viewport renders next frame.
#[coverage(off)]
fn drain_pane_completions_into_runtime(
    ui: &mut WorkspaceUi,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    geometry: Geometry,
) {
    while let Ok(completion) = ui.pane_completions.try_recv() {
        if let Some(agent) = ui.agent.as_mut() {
            agent.port = Some(completion.port);
        }
        let (operation, result) = match completion.outcome {
            PaneLaunchOutcome::Agent { operation, result }
            | PaneLaunchOutcome::Terminal { operation, result } => (operation, result),
        };
        let Some(target) = pending_targets.remove(&operation) else {
            continue;
        };
        match result {
            Ok(terminal) => {
                let _ = runtime.complete_pane(target, operation, terminal.clone());
                let _ = runtime.focus_terminal(target, terminal.clone());
                ui.start_terminal_session(terminal, geometry);
            }
            Err(message) => {
                let _ = runtime.fail_pane(target, operation, message);
            }
        }
    }
}

/// Controller-driven real-terminal frame loop (`drain → poll → render → input →
/// dispatch`). Home row state, live-pane availability, and the Home frame come
/// from [`WorkspaceRuntime`]/`render_home`; the legacy [`WorkspaceUi`] is kept as
/// the daemon IO transport (session workers, pane launches, terminal streams,
/// metrics). This is the controller replacement for
/// `drive_workspace_with_agent_port_and_selection_mode`; the composition root
/// switches to it separately.
#[coverage(off)]
#[allow(clippy::too_many_arguments)]
fn drive_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
    agent_port: Box<dyn AgentCommandPort>,
    metrics_port: Box<dyn MetricsPort>,
    mut pr_port: Box<dyn PrSnapshotPort>,
    mut browser: Box<dyn BrowserOpener>,
) -> io::Result<WorkspaceStep> {
    let workspace_id = snapshot.workspace_id;
    let session_ids = snapshot.session_ids.clone();
    let workspace_name = snapshot.workspace.name.clone();
    let root_cwd = snapshot.workspace.path.clone();
    let workspace =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, session_ids.clone());
    let mut ui = WorkspaceUi::new(workspace, session_commands)
        .with_agent_context(workspace_id, session_ids.clone(), agent_port)
        .with_metrics_port(metrics_port);
    let mut runtime = WorkspaceRuntime::new(workspace_id, session_ids);
    let mut pending_targets: std::collections::HashMap<OperationId, Target> =
        std::collections::HashMap::new();
    loop {
        drain_session_completions(&mut ui);
        sync_runtime_sessions(&mut runtime, &ui);
        refresh_metrics(&mut ui);
        let (height, width) = term.size()?;
        ui.set_terminal_size(height, width);
        let geometry = terminal_geometry(height, width);
        drain_pane_completions_into_runtime(&mut ui, &mut runtime, &mut pending_targets, geometry);
        ui.resize_terminals(geometry);
        let terminal_view = controller_terminal_view(&mut ui, &runtime);
        let sessions = project_controller_sessions(&ui);
        let metrics = ui.workspace.metrics();
        let frame = render_controller_frame(
            height,
            width,
            &runtime,
            &workspace_name,
            &root_cwd,
            &sessions,
            metrics,
            ui.workspace.git_diffs(),
            terminal_view,
        );
        term.draw(&frame)?;
        drain_pane_launches(&mut ui, geometry);
        let key = term.read_key()?;
        if runtime.wants_live_input()
            && let Some(terminal) = runtime.focused_terminal()
            && let Some(bytes) = key_to_terminal_bytes(key.clone())
        {
            ui.send_terminal_bytes(&terminal, &bytes);
            continue;
        }
        // Overview command palette (`:`) is not yet ported to the controller loop.
        if key == Key::Char(':') {
            continue;
        }
        for effect in runtime.handle_key(key) {
            runtime.on_effect(&effect);
            for event in controller_overlay_events(
                pr_port.as_mut(),
                browser.as_mut(),
                &workspace_name,
                &root_cwd,
                &sessions,
                &effect,
            ) {
                let _ = runtime.apply_event(event);
            }
            if dispatch_controller_effect(&mut ui, &effect, &mut pending_targets)
                == ControllerFlow::Exit
            {
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
#[coverage(off)]
#[allow(clippy::too_many_arguments)]
pub fn run_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
    agent_port: Box<dyn AgentCommandPort>,
    metrics_port: Box<dyn MetricsPort>,
    pr_port: Box<dyn PrSnapshotPort>,
    browser: Box<dyn BrowserOpener>,
) -> io::Result<Exit> {
    drive_workspace_controller(
        term,
        snapshot,
        session_commands,
        agent_port,
        metrics_port,
        pr_port,
        browser,
    )
    .map(|_| Exit::Quit)
}

fn refresh_metrics(ui: &mut WorkspaceUi) {
    if let Some(metrics) = ui.metrics_port.latest() {
        ui.workspace.set_metrics(Some(metrics));
    }
    let sessions = ui
        .workspace
        .sessions()
        .iter()
        .zip(ui.workspace.session_ids())
        .map(|(session, id)| (*id, session.root.clone()))
        .collect::<Vec<_>>();
    ui.workspace
        .set_git_diffs(ui.metrics_port.git_diffs(&sessions));
}

/// Open list 用に、registry の生値と recent projection を結び付ける。
///
/// `Recent::Workspace` は各登録 workspace の集計済み表示値を持つ。互換呼び出しで
/// projection が無いときだけ、生値から 0 件の overview を組み立てる。
#[coverage(off)]
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
#[coverage(off)]
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
#[coverage(off)]
#[allow(clippy::too_many_arguments)]
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
#[coverage(off)]
#[allow(clippy::too_many_arguments)]
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
#[coverage(off)]
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
#[coverage(off)]
fn open_snapshot_via_controller<'a, 'b>(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: &mut dyn SessionCommandPortFactory,
    agent_commands: Option<&mut (dyn AgentCommandPortFactory + 'a)>,
    metrics: Option<&mut (dyn MetricsPortFactory + 'b)>,
) -> io::Result<WorkspaceStep> {
    let agent_port = agent_commands.map_or_else(
        || -> Box<dyn AgentCommandPort> { Box::new(UnavailableAgentCommandPort) },
        AgentCommandPortFactory::create,
    );
    let metrics_port = metrics.map_or_else(
        || -> Box<dyn MetricsPort> { Box::new(NoMetrics) },
        MetricsPortFactory::create,
    );
    drive_workspace_controller(
        term,
        snapshot,
        session_commands.create(),
        agent_port,
        metrics_port,
        Box::new(UnavailablePrSnapshotPort),
        Box::new(UnavailableBrowserOpener),
    )
}

// The screen graph is an IO composition boundary.  Its choices are covered by
// the injected loader/port tests; LLVM coverage excludes only this terminal
// loop, consistently with the existing `run_with_settings` entry point.
#[coverage(off)]
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
                WelcomeStep::ConfigScreen => screen = Screen::Config,
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
                        session_commands,
                        agent_commands.as_deref_mut(),
                        metrics.as_deref_mut(),
                    )?;
                    if workspace_step == WorkspaceStep::Quit {
                        return Ok(Exit::Quit);
                    }
                }
            },
            Screen::Open => match step_open(&mut open, key) {
                OpenStep::Stay => {}
                OpenStep::Quit => return Ok(Exit::Quit),
                OpenStep::Back => screen = Screen::Welcome,
                OpenStep::Choose(paths) => {
                    for path in paths {
                        let snapshot = loader.open(&path)?;
                        welcome.record_opened(&snapshot.workspace);
                        open.record_opened(&snapshot.workspace);
                        let workspace_step = open_snapshot_via_controller(
                            term,
                            snapshot,
                            session_commands,
                            agent_commands.as_deref_mut(),
                            metrics.as_deref_mut(),
                        )?;
                        if workspace_step == WorkspaceStep::Quit {
                            return Ok(Exit::Quit);
                        }
                    }
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
            },
            Screen::Config => match step_config(&mut config_form, key, settings) {
                ConfigStep::Stay => {}
                ConfigStep::Quit => return Ok(Exit::Quit),
                ConfigStep::Back => screen = Screen::Welcome,
                ConfigStep::Saved => {
                    let (height, width) = term.size()?;
                    term.draw(&config::render(height, width, &config_form))?;
                    screen = Screen::Welcome;
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
#[coverage(off)]
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
#[coverage(off)]
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
    #[coverage(off)]
    fn read(
        &mut self,
        _scope: usagi_core::usecase::settings::SettingsScope,
    ) -> io::Result<usagi_core::domain::settings::Settings> {
        Ok(usagi_core::domain::settings::Settings::default())
    }

    #[coverage(off)]
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
    #[coverage(off)]
    pub fn new(out: &'a mut W, info: &'a AppInfo) -> Self {
        Self { out, info }
    }

    /// 画面を識別する `label` をアプリ情報とともに一行で書き出す。
    #[coverage(off)]
    fn write_screen(&mut self, label: &str) -> io::Result<()> {
        writeln!(self.out, "{}: {label}", self.info.describe())
    }
}

impl<W: Write + ?Sized> ScreenRunner for BannerScreenRunner<'_, W> {
    #[coverage(off)]
    fn welcome(&mut self) -> io::Result<()> {
        self.write_screen("welcome TUI")
    }

    #[coverage(off)]
    fn workspace(&mut self, path: &Path) -> io::Result<()> {
        self.write_screen(&format!("workspace TUI ({})", path.display()))
    }

    #[coverage(off)]
    fn config(&mut self) -> io::Result<()> {
        self.write_screen("config TUI")
    }

    #[coverage(off)]
    fn doctor(&mut self) -> io::Result<()> {
        self.write_screen("doctor TUI")
    }
}

#[cfg(test)]
#[coverage(off)] // Test assertion branches are not product coverage targets.
mod tests {
    use super::{
        AgentCommandPort, AgentCommandPortFactory, BannerScreenRunner, Config, ConfigStep,
        DefaultSettingsPort, Exit, Geometry, MetricsPort, MetricsPortFactory, NewStep, NoMetrics,
        NoMetricsFactory, SessionCommandPort, SessionCommandPortFactory, SessionCommandResult,
        Start, TerminalError, UnavailableBrowserOpener, UnavailablePrSnapshotPort,
        UnavailableSessionCommandPort, WelcomeStep, WorkspaceLoader, WorkspaceSnapshot,
        app_event_from_key, key_to_terminal_bytes, play_startup_splash, render_controller_frame,
        render_home_snapshot, run as run_from_start, run_with_settings,
        run_with_settings_and_agent_and_metrics_port_factory_and_model_availability,
        run_workspace_controller, step_config, step_new, terminal_geometry, welcome_action,
        write_banner,
    };
    use crate::presentation::views::config::AvailableAgentModels;
    use crate::presentation::views::new::{Field, Mode, New};
    use crate::presentation::views::welcome::MenuAction;

    use crate::usecase::application::controller::{AppEvent, AppKey};
    use crate::usecase::application::run as dispatch;
    use crate::usecase::application::{EntryScreen, Key, Terminal};
    use crate::usecase::overview::SessionCommand;
    use crate::usecase::terminal_input::LiveTerminalAction;
    use chrono::{DateTime, Duration, Utc};
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::agent::AgentProfileId;
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::note::Scratchpad;

    use usagi_core::domain::recent::{Recent, UniteOverview};
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};

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
        // Raw passthrough, pointer cells, and clicks never reach the Home reducer.
        assert_eq!(app_event_from_key(Key::Passthrough(vec![0x1b])), None);
        assert_eq!(app_event_from_key(Key::Click { column: 3, row: 4 }), None);
        // Left/Right and Ctrl-D carry no Home management meaning.
        assert_eq!(app_event_from_key(Key::Left), None);
        assert_eq!(app_event_from_key(Key::Right), None);
        assert_eq!(app_event_from_key(Key::CtrlD), None);
        // Tab close and terminal scroll/copy stay pane- and shell-level concerns.
        for action in [
            LiveTerminalAction::CloseTab,
            LiveTerminalAction::ScrollUp,
            LiveTerminalAction::ScrollDown,
            LiveTerminalAction::CopyTerminalSelection,
        ] {
            assert_eq!(app_event_from_key(Key::Live(action)), None);
        }
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

    fn snapshot(name: &str) -> WorkspaceSnapshot {
        WorkspaceSnapshot::new(ws(name), state(name))
    }

    type SessionCommandCall = (String, Option<String>, SessionCommand);

    struct SuccessfulAgentPort(TerminalRef);

    impl AgentCommandPort for SuccessfulAgentPort {
        fn launch(
            &mut self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _profile: Option<AgentProfileId>,
        ) -> Result<TerminalRef, String> {
            Ok(self.0.clone())
        }
    }

    /// screen graph の workspace 遷移が実 port を通すことを検証する fake port。
    /// `session create <name>` に対しては、daemon lifecycle snapshot を模して
    /// `name` の session row を返し、sidebar への反映まで観測できるようにする。
    #[derive(Clone)]
    struct SnapshotSessionPort(Arc<Mutex<Vec<SessionCommandCall>>>);

    impl SessionCommandPort for SnapshotSessionPort {
        #[coverage(off)]
        fn execute(
            &mut self,
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
        #[coverage(off)]
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
        use crate::usecase::application::controller::{AppEvent, AppKey};

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
        };
        let sessions = std::slice::from_ref(&projected);
        let git = std::collections::BTreeMap::new();
        let root = std::path::Path::new("/work");

        // Base Home frame: workspace name and session row render.
        let runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let base =
            render_controller_frame(20, 80, &runtime, "atlas", root, sessions, None, &git, None);
        assert!(base.join("\n").contains("atlas"));
        assert!(base.join("\n").contains("alpha"));

        // Create form overlay: with no sessions a single Down reaches + new session.
        let mut creating = WorkspaceRuntime::new(workspace, Vec::new());
        let _ = creating.handle_key(Key::Down);
        let _ = creating.handle_key(Key::Enter);
        let create =
            render_controller_frame(20, 80, &creating, "atlas", root, &[], None, &git, None);
        assert!(create.join("\n").contains("New session"));

        // Quit confirmation overlay.
        let mut quitting = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = quitting.apply_event(AppEvent::Key(AppKey::CtrlQ));
        let quit =
            render_controller_frame(20, 80, &quitting, "atlas", root, sessions, None, &git, None);
        assert!(quit.join("\n").contains("Detach from this workspace?"));
    }

    #[test]
    #[coverage(off)]
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
    }

    #[test]
    #[coverage(off)]
    fn controller_loop_opens_the_create_form_from_the_new_session_row() {
        // An empty workspace shows only root and `+ new session`, so one Down
        // reaches the create entry deterministically.
        let snapshot = WorkspaceSnapshot::new(
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
            Box::new(SuccessfulAgentPort(terminal)),
            Box::new(NoMetrics),
            Box::new(UnavailablePrSnapshotPort),
            Box::new(UnavailableBrowserOpener),
        );

        assert!(matches!(result, Ok(Exit::Quit)));
        // The Overlay::CreateSession form rendered with the typed name, confirming
        // the `+ new session` create-entry seam works through the controller loop.
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("New session"))
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("api"))
        );
    }

    /// テスト用 Terminal。キー列を順に返し、描いたフレームを記録する。
    #[derive(Default)]
    struct FakeTerminal {
        keys: VecDeque<Key>,
        frames: Vec<Vec<String>>,
        waits: Vec<std::time::Duration>,
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
    }

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
            self.keys
                .pop_front()
                .ok_or_else(|| io::Error::other("no more keys"))
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
            _session: SessionId,
            _profile: Option<AgentProfileId>,
        ) -> Result<TerminalRef, String> {
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
            .launch(WorkspaceId::new(), SessionId::new(), None)
            .unwrap_err();

        assert_eq!(error, "not launched in this test");
        assert_eq!(
            port.launch_terminal(
                WorkspaceId::new(),
                SessionId::new(),
                Geometry { cols: 80, rows: 24 },
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
        fail: bool,
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
        assert!(matches!(
            step_config(&mut config, Key::Enter, &mut settings),
            ConfigStep::Saved
        ));
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
        assert_eq!(form.url(), "a");
        assert!(matches!(step_new(&mut form, Key::Enter), NewStep::Stay));
        assert!(matches!(step_new(&mut form, Key::Other), NewStep::Stay));
        assert!(matches!(step_new(&mut form, Key::Escape), NewStep::Back));
        assert!(matches!(step_new(&mut form, Key::Quit), NewStep::Quit));
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
            _session: SessionId,
            _profile: Option<AgentProfileId>,
        ) -> Result<TerminalRef, String> {
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
            port.launch(WorkspaceId::new(), SessionId::new(), None)
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
                SessionId::new(),
                Geometry { cols: 80, rows: 24 },
            ),
            Err("terminal launch is unavailable".to_owned())
        );
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
        assert_eq!(key_to_terminal_bytes(Key::Left), Some(b"\x1b[D".to_vec()));
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

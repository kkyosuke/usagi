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
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::AgentProfileId;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId};
use usagi_core::domain::recent::Recent;
use usagi_core::domain::settings::{DefaultModel, ModalSelectionMode};
use usagi_core::domain::workspace::Workspace;
#[cfg(not(test))]
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::usecase::client::DaemonMetrics;

use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::views::closeup_modal::{self, CloseupModal};
use crate::presentation::views::config::{self, AvailableAgentModels, Config};
use crate::presentation::views::new::{self, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::overview_modal::{self, OverviewModal};
use crate::presentation::views::pr_modal::{self, PrModal};
use crate::presentation::views::remove_modal::{self, RemoveModal};
use crate::presentation::views::splash;
use crate::presentation::views::text_overlay::{self, OverlayDocument, TextOverlay};
use crate::presentation::views::welcome::{self, MenuAction, Welcome};
use crate::presentation::views::workspace::{self, GitDiff, Mode, Workspace as WorkspaceView};
use crate::presentation::widgets::modal::{self, ConfirmationModal, ConfirmationView};
use crate::usecase::application::controller::{AppEvent, AppKey};
use crate::usecase::application::pane::{PaneKind, PaneSelection, TabSelection};
use crate::usecase::application::pane_runtime::Geometry;
use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};
use crate::usecase::application::terminal_session::{
    TerminalAttach, TerminalChunk, TerminalError, TerminalSession, TerminalStreamPort,
};
use crate::usecase::application::{Key, ScreenRunner, Terminal};
use crate::usecase::closeup;
use crate::usecase::overview::{self, SessionCommand};
use crate::usecase::terminal_input::{LiveTerminalAction, PointerEvent, PointerKind};
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
    Stay,
    Quit,
}

/// Workspace の基底画面より手前に重ねる modal。
///
/// Closeup の action menu は [`Mode::Closeup`] の既定 surface なのでここには含めず、
/// Overview / PR を一時的に最前面へ出すときだけこの値を使う。
enum WorkspaceModal {
    Overview(OverviewModal),
    Remove(RemoveModal),
    Pr(PrModal),
    Text(TextOverlay),
    /// A safe operation failure. Unlike document overlays, every user input
    /// dismisses this acknowledgement dialog.
    Error(TextOverlay),
    Quit(QuitModal),
}

/// The result of a workspace-exit confirmation. Closing the TUI leaves the
/// daemon alone; ending the workspace first asks it to stop its live sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitAction {
    CloseTui,
    EndWorkspace,
}

#[derive(Debug, Clone, Copy)]
struct QuitModal {
    action: QuitAction,
    confirmation: ConfirmationModal,
}

impl QuitModal {
    #[coverage(off)]
    const fn new(action: QuitAction) -> Self {
        Self {
            action,
            confirmation: ConfirmationModal::new(),
        }
    }
}

/// Home overlay が必要とするデータを取得する境界。
///
/// backend の diff / PR fetch はこの port の実装側へ閉じる。返す文字列はすべて
/// 画面に表示して安全な要約でなければならず、生の command error や credential を
/// 渡してはならない。現在の runtime は snapshot だけから読める値を提供し、未接続の
/// diff は安全な fallback を返す。
pub trait OverlayDataPort {
    /// Preview の安全な表示内容を返す。
    fn preview(&self, workspace: &WorkspaceView) -> OverlayDocument;
    /// Diff の安全な表示内容を返す。
    fn diff(&self, workspace: &WorkspaceView) -> OverlayDocument;
    /// 長文 text の安全な表示内容を返す。
    fn text(&self, workspace: &WorkspaceView) -> OverlayDocument;
    /// Pull Request 一覧または安全な fallback message を返す。
    ///
    /// # Errors
    ///
    /// データを取得できず、画面へ表示して安全な fallback message を返す場合に失敗する。
    fn pull_requests(
        &self,
        workspace: &WorkspaceView,
    ) -> Result<Vec<usagi_core::domain::pullrequest::PrLink>, String>;
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

/// 永続化済み snapshot を読む既定の overlay data port。
struct SnapshotOverlayData;

impl OverlayDataPort for SnapshotOverlayData {
    #[coverage(off)]
    fn preview(&self, workspace: &WorkspaceView) -> OverlayDocument {
        OverlayDocument::Ready(workspace.focused_preview_lines())
    }

    #[coverage(off)]
    fn diff(&self, _workspace: &WorkspaceView) -> OverlayDocument {
        OverlayDocument::Unavailable(
            "Diff data is unavailable until a backend supplies it.".to_string(),
        )
    }

    #[coverage(off)]
    fn text(&self, workspace: &WorkspaceView) -> OverlayDocument {
        let lines = workspace.focused_note_lines();
        if lines.is_empty() {
            OverlayDocument::Unavailable("No notes are available for this target.".to_string())
        } else {
            OverlayDocument::Ready(lines)
        }
    }

    #[coverage(off)]
    fn pull_requests(
        &self,
        workspace: &WorkspaceView,
    ) -> Result<Vec<usagi_core::domain::pullrequest::PrLink>, String> {
        Ok(workspace.focused_prs().to_vec())
    }
}

/// Workspace runtime が 1 フレームを進めるために持つ presentation state。
///
/// 永続化済み workspace state は [`WorkspaceView`] が持ち、ここでは top-level mode、
/// Closeup の選択、最前面 modal を組み合わせる。端末 IO は持たない。
struct WorkspaceUi {
    workspace: WorkspaceView,
    closeup: CloseupModal,
    modal_selection_mode: ModalSelectionMode,
    modal: Option<WorkspaceModal>,
    /// Closeup に tab があるときでも action modal を前面へ出す明示要求。tab が無い
    /// Closeup では常に modal が出るため、このフラグは tab がある間だけ意味を持つ。
    /// `Ctrl-O a`（[`LiveTerminalAction::OpenCloseupModal`]）で立て、Switch へ戻る・
    /// action を選ぶ・modal を閉じると倒す。
    closeup_action_forced: bool,
    overlay_data: Box<dyn OverlayDataPort>,
    /// A create owns the port in its worker until completion, preventing a
    /// second lifecycle request while its sidebar skeleton is visible.
    session_commands: Option<Box<dyn SessionCommandPort>>,
    session_completions: Receiver<SessionCommandCompletion>,
    session_completion_sender: Sender<SessionCommandCompletion>,
    /// Name of a create whose successful completion may still take focus. Any
    /// later user input clears this, leaving the user on their current surface.
    create_auto_closeup: Option<String>,
    skeleton_frame: usize,
    metrics_port: Box<dyn MetricsPort>,
    agent: Option<AgentContext>,
    pane_launches: Vec<PaneLaunch>,
    pane_completions: Receiver<PaneLaunchCompletion>,
    pane_completion_sender: Sender<PaneLaunchCompletion>,
    /// Live coordinators for daemon-owned terminals opened in this workspace,
    /// one per live terminal tab.  Detached/closed tabs are pruned lazily.
    terminals: Vec<TerminalSession>,
    terminal_selection: Option<TerminalSelection>,
    pending_terminal_pointer: Option<TerminalPoint>,
    dragging_terminal_selection: bool,
    auto_scroll_terminal_selection: Option<bool>,
    pending_clipboard_text: Option<String>,
    terminal_size: (usize, usize),
}

/// The maximum gap between two presses on the same sidebar session row.
const SIDEBAR_DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Apply a sidebar click before regular key dispatch. A single click only moves
/// the cursor; a double click on a session opens that session's Closeup, exactly
/// as selecting it and pressing Enter would. Overlays and inline creation own
/// the pointer, so background rows cannot be changed through them.
#[coverage(off)]
fn handle_sidebar_click(
    ui: &mut WorkspaceUi,
    height: usize,
    width: usize,
    key: &Key,
    previous: &mut Option<(usize, Instant)>,
) -> bool {
    let Key::Click { column, row } = key else {
        return false;
    };
    ui.workspace.record_interaction();
    // This path returns before `step_workspace`, so record a genuine pointer
    // interaction here as well. A later lifecycle completion must not replace
    // a user's explicit navigation choice.
    if ui.workspace.pending_session().is_some() {
        ui.create_auto_closeup = None;
    }
    if usize::from(*column) >= workspace::right_pane_left(width) {
        return false;
    }
    if ui.modal.is_some() || ui.closeup_modal_visible() || ui.workspace.creating_session_inline() {
        *previous = None;
        return true;
    }
    let Some(index) = workspace::sidebar_row_at(
        height,
        width,
        &ui.workspace,
        ui.skeleton_frame,
        *column,
        *row,
    ) else {
        *previous = None;
        return true;
    };
    // The root and persistent create action retain their existing keyboard
    // grammar. This gesture is intentionally limited to real session rows.
    if index == 0 || index > ui.workspace.sessions().len() {
        *previous = None;
        return true;
    }
    let now = Instant::now();
    let doubled = previous.is_some_and(|(previous_index, at)| {
        previous_index == index && now.duration_since(at) <= SIDEBAR_DOUBLE_CLICK
    });
    *previous = (!doubled).then_some((index, now));
    ui.workspace.select_row(index);
    if doubled {
        ui.enter_closeup();
    }
    true
}

struct AgentContext {
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    /// A launch worker temporarily owns this port. Terminal streaming resumes
    /// only after the worker returns it with the daemon result.
    port: Option<Box<dyn AgentCommandPort>>,
    default_profile: AgentProfileId,
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
    Diff {
        operation: OperationId,
    },
    Fail {
        operation: OperationId,
        message: String,
    },
}

impl WorkspaceUi {
    #[cfg(test)]
    #[coverage(off)]
    fn with_overlay_data(workspace: WorkspaceView, overlay_data: Box<dyn OverlayDataPort>) -> Self {
        Self::with_ports_and_selection_mode(
            workspace,
            overlay_data,
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        )
    }

    #[coverage(off)]
    fn with_ports_and_selection_mode(
        workspace: WorkspaceView,
        overlay_data: Box<dyn OverlayDataPort>,
        session_commands: Box<dyn SessionCommandPort>,
        modal_selection_mode: ModalSelectionMode,
    ) -> Self {
        let closeup =
            CloseupModal::with_selection_mode(workspace.focused_label(), modal_selection_mode);
        let (session_completion_sender, session_completions) = mpsc::channel();
        let (pane_completion_sender, pane_completions) = mpsc::channel();
        Self {
            workspace,
            closeup,
            modal_selection_mode,
            modal: None,
            closeup_action_forced: false,
            overlay_data,
            session_commands: Some(session_commands),
            session_completions,
            session_completion_sender,
            create_auto_closeup: None,
            skeleton_frame: 0,
            metrics_port: Box::new(NoMetrics),
            agent: None,
            pane_launches: Vec::new(),
            pane_completions,
            pane_completion_sender,
            terminals: Vec::new(),
            terminal_selection: None,
            pending_terminal_pointer: None,
            dragging_terminal_selection: false,
            auto_scroll_terminal_selection: None,
            pending_clipboard_text: None,
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
        default_model: DefaultModel,
    ) -> Self {
        self.agent = Some(AgentContext {
            workspace,
            sessions,
            port: Some(port),
            default_profile: AgentProfileId::new(default_model.profile_id())
                .expect("default model profile IDs are canonical"),
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

    /// Polls the focused live terminal once and projects its screen rows into
    /// the view.  A non-terminal or unattached selection clears the projection.
    #[coverage(off)]
    fn refresh_terminal(&mut self) {
        let Some(terminal) = self.workspace.focused_live_terminal().cloned() else {
            self.workspace.set_terminal_view(None);
            return;
        };
        let rows = if let (Some(port), Some(session)) = (
            self.agent
                .as_mut()
                .and_then(|agent| agent.port.as_deref_mut()),
            self.terminals
                .iter_mut()
                .find(|session| session.terminal().fences(&terminal)),
        ) {
            session.poll(&mut AgentStreamPort(port));
            Some(self.terminal_selection.as_ref().map_or_else(
                || session.display_rows_with_scrollback(),
                |selection| session.display_rows_with_scrollback_selection(selection),
            ))
        } else {
            None
        };
        self.workspace.set_terminal_view(rows);
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

    #[coverage(off)]
    fn terminal_selection_at(&mut self, point: TerminalPoint) {
        let Some(terminal) = self.workspace.focused_live_terminal().cloned() else {
            return;
        };
        if let Some(session) = self
            .terminals
            .iter()
            .find(|session| session.terminal().fences(&terminal))
        {
            self.terminal_selection = Some(session.begin_selection(point));
            self.workspace
                .set_terminal_feedback(Some("terminal selection started".to_owned()));
        }
    }

    #[coverage(off)]
    fn extend_terminal_selection(&mut self, point: TerminalPoint) {
        if let Some(selection) = &mut self.terminal_selection {
            selection.extend(point);
        }
    }

    #[coverage(off)]
    fn queue_terminal_copy(&mut self) {
        let Some(selection) = &self.terminal_selection else {
            self.workspace
                .set_terminal_feedback(Some("no terminal text is selected".to_owned()));
            return;
        };
        let text = selection.text();
        if text.is_empty() {
            self.workspace
                .set_terminal_feedback(Some("no terminal text is selected".to_owned()));
        } else {
            self.pending_clipboard_text = Some(text);
        }
    }

    #[coverage(off)]
    fn advance_terminal_auto_scroll(&mut self) {
        let Some(older) = self.auto_scroll_terminal_selection else {
            return;
        };
        if !self.dragging_terminal_selection {
            return;
        }
        if older {
            self.workspace.terminal_scroll_up();
        } else {
            self.workspace.terminal_scroll_down();
        }
        if let Some(point) = workspace::terminal_edge_point(
            self.terminal_size.0,
            self.terminal_size.1,
            &self.workspace,
            older,
        ) {
            self.extend_terminal_selection(point);
        }
    }

    fn take_terminal_copy(&mut self) -> Option<String> {
        self.pending_clipboard_text.take()
    }

    /// Routes a keystroke to the focused live terminal.  Returns `true` when the
    /// key was consumed by the terminal so the workspace does not also act on it.
    #[coverage(off)]
    fn forward_terminal_input(&mut self, key: &Key) -> bool {
        // A live tab is only the input owner while Closeup exposes it.  In
        // particular, Ctrl-O o must return to Switch before the next ordinary
        // key can reach the previously focused PTY.
        if self.workspace.mode() != Mode::Closeup {
            return false;
        }
        let Some(terminal) = self.workspace.focused_live_terminal().cloned() else {
            return false;
        };
        let Some(bytes) = key_to_terminal_bytes(key.clone()) else {
            return false;
        };
        if let (Some(port), Some(session)) = (
            self.agent
                .as_mut()
                .and_then(|agent| agent.port.as_deref_mut()),
            self.terminals
                .iter_mut()
                .find(|session| session.terminal().fences(&terminal)),
        ) {
            session.send_input(&mut AgentStreamPort(port), &bytes);
        }
        true
    }

    /// Close only the focused pane and release its client-side subscription.
    ///
    /// Closing is a view concern: the daemon keeps the terminal alive, while
    /// this TUI detaches from its stream.  A still-queued launch is removed
    /// before it can create a detached daemon terminal after its placeholder
    /// has disappeared.
    #[coverage(off)]
    fn close_focused_pane(&mut self) {
        let selection = self.workspace.pane().selected().clone();
        let live_terminal = self.workspace.focused_live_terminal().cloned();
        self.workspace.close_pane();

        if let PaneSelection::Tab(TabSelection::Pending(operation)) = selection {
            self.pane_launches.retain(|launch| match launch {
                PaneLaunch::Agent {
                    operation: queued, ..
                }
                | PaneLaunch::Terminal {
                    operation: queued, ..
                }
                | PaneLaunch::Diff { operation: queued }
                | PaneLaunch::Fail {
                    operation: queued, ..
                } => *queued != operation,
            });
        }

        if let Some(terminal) = live_terminal {
            if let Some(port) = self
                .agent
                .as_mut()
                .and_then(|agent| agent.port.as_deref_mut())
                && let Some(session) = self
                    .terminals
                    .iter_mut()
                    .find(|session| session.terminal().fences(&terminal))
            {
                session.detach(&mut AgentStreamPort(port));
            }
            self.terminals
                .retain(|session| !session.terminal().fences(&terminal));
        }
    }

    /// 選択中の行を対象に Closeup へ入り、action menu を先頭から開く。
    ///
    /// tab が無い target では action modal がそのまま前面に出る。tab がある target
    /// では tab を前面にするため、`closeup_action_forced` は倒したまま入る。
    #[coverage(off)]
    fn enter_closeup(&mut self) {
        self.open_closeup(false);
    }

    /// 選択中の target の Closeup action modal を前面にして開く。
    ///
    /// Switch からの live prefix でも Closeup 内からの再オープンでも同じ遷移を
    /// 使う。前の target の action selection や forced state を残さない。
    #[coverage(off)]
    fn open_closeup_action(&mut self) {
        self.open_closeup(true);
    }

    #[coverage(off)]
    fn open_closeup(&mut self, force_action: bool) {
        self.workspace.enter_closeup();
        self.closeup = CloseupModal::with_selection_mode(
            self.workspace.focused_label(),
            self.modal_selection_mode,
        );
        self.modal = None;
        self.closeup_action_forced = force_action;
    }

    /// Switch へ戻り、Closeup の前面状態を残さない。
    #[coverage(off)]
    fn enter_switch(&mut self) {
        self.workspace.enter_switch();
        self.modal = None;
        self.closeup_action_forced = false;
    }

    /// Closeup の action modal が現在前面に出ているか。tab が無ければ常に出る。tab が
    /// あるときは `Ctrl-O a` で明示要求した間だけ出る。
    #[coverage(off)]
    fn closeup_modal_visible(&self) -> bool {
        self.workspace.mode() == Mode::Closeup
            && (!self.workspace.has_panes() || self.closeup_action_forced)
    }

    /// 現在 mode を保ったまま Workspace scope の command palette を重ねる。
    #[coverage(off)]
    fn open_overview(&mut self) {
        self.modal = Some(WorkspaceModal::Overview(
            OverviewModal::with_selection_mode(self.modal_selection_mode),
        ));
    }

    /// Open the v1-compatible checklist over the current daemon snapshot.
    /// The modal captures records, not row indexes; dispatch performs another
    /// incarnation check before it asks the daemon to remove anything.
    #[coverage(off)]
    fn open_remove_selector(&mut self, force: bool) {
        self.modal = Some(WorkspaceModal::Remove(RemoveModal::new(
            self.workspace.sessions().to_vec(),
            force,
        )));
    }

    #[coverage(off)]
    fn open_quit_confirmation(&mut self, action: QuitAction) {
        self.modal = Some(WorkspaceModal::Quit(QuitModal::new(action)));
    }

    /// Replace the active command palette with a readable failure dialog.
    /// The palette result band is one line wide, so it cannot safely present a
    /// remedial daemon error without clipping it.
    #[coverage(off)]
    fn show_error_dialog(&mut self, message: &str) {
        self.modal = Some(WorkspaceModal::Error(
            TextOverlay::new(
                Role::Danger
                    .style()
                    .bold()
                    .paint("\u{f06a} Session operation failed"),
                OverlayDocument::Ready(crate::presentation::widgets::wrap_to_width(
                    message,
                    text_overlay::INNER_WIDTH,
                )),
            )
            .acknowledgement(),
        ));
    }

    /// Snapshot reconciliation may remove the Closeup target. Rebuild its
    /// display label and action focus from the surviving sidebar projection
    /// before the selector gives input back to Closeup.
    #[coverage(off)]
    fn refresh_closeup_after_session_snapshot(&mut self) {
        if self.workspace.mode() == Mode::Closeup {
            self.closeup = CloseupModal::with_selection_mode(
                self.workspace.focused_label(),
                self.modal_selection_mode,
            );
            self.closeup_action_forced = false;
        }
    }

    /// 選択中セッションの PR 一覧を現在 mode の上へ重ねる。root は空一覧になる。
    #[coverage(off)]
    fn open_prs(&mut self) {
        self.modal = Some(match self.overlay_data.pull_requests(&self.workspace) {
            Ok(prs) => WorkspaceModal::Pr(PrModal::new(prs)),
            Err(message) => WorkspaceModal::Text(TextOverlay::new(
                "Pull Request",
                OverlayDocument::Unavailable(message),
            )),
        });
    }

    #[coverage(off)]
    fn open_preview(&mut self) {
        let document = self.overlay_data.preview(&self.workspace);
        self.modal = Some(WorkspaceModal::Text(TextOverlay::new("Preview", document)));
    }

    #[coverage(off)]
    fn open_diff(&mut self) {
        let operation = self.workspace.open_pane(PaneKind::Diff);
        self.pane_launches.push(PaneLaunch::Diff { operation });
        self.closeup_action_forced = false;
    }

    #[coverage(off)]
    fn open_text(&mut self) {
        let document = self.overlay_data.text(&self.workspace);
        self.modal = Some(WorkspaceModal::Text(TextOverlay::new("Notes", document)));
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

/// New 画面のキー処理（純粋）。上下でフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_new(form: &mut New, key: Key) -> NewStep {
    match key {
        Key::Up | Key::Char('k') => {
            form.focus_prev();
            NewStep::Stay
        }
        Key::Down | Key::Char('j') => {
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
        Key::Enter
        | Key::Tab
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

/// Overview modal の入力処理。文字入力中の `q` を含め、modal が全キーを先に受け取る。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_overview_command(ui: &mut WorkspaceUi, key: Key) -> bool {
    if ui.session_commands.is_none() {
        return key == Key::Escape;
    }
    let WorkspaceModal::Overview(modal) = ui.modal.as_mut().expect("overview modal is active")
    else {
        return false;
    };
    match key {
        Key::Up => {
            if modal.selection_mode() == ModalSelectionMode::Prompt && modal.recall_previous() {
            } else {
                modal.select_prev();
            }
        }
        Key::Down => {
            if modal.selection_mode() == ModalSelectionMode::Prompt && modal.recall_next() {
            } else {
                modal.select_next();
            }
        }
        Key::Left if modal.selection_mode() == ModalSelectionMode::Action => {
            modal.collapse();
        }
        Key::Right if modal.selection_mode() == ModalSelectionMode::Action => {
            modal.expand_selected();
        }
        Key::Left => modal.cursor_left(),
        Key::Right => modal.cursor_right(),
        Key::Backspace => modal.backspace(),
        Key::Tab => modal.complete_selected(),
        Key::Char(ch) => modal.insert_char(ch),
        Key::Escape => return true,
        Key::Enter => {
            let input = modal.submission();
            modal.record_submission();
            match overview::interpret(&input) {
                Ok(overview::Command::Session { arguments }) => {
                    match overview::parse_session(&arguments) {
                        Ok(command @ SessionCommand::Create { .. }) => {
                            begin_session_create(ui, command);
                        }
                        Ok(SessionCommand::SelectRemove { force }) => {
                            ui.open_remove_selector(force);
                        }
                        Ok(command) => match ui
                            .session_commands
                            .as_mut()
                            .expect("session port is available")
                            .execute(
                                ui.workspace.record(),
                                ui.workspace.selected_session(),
                                command,
                            ) {
                            Ok(result) => apply_session_result(ui, result),
                            Err(error) => modal.set_error(error),
                        },
                        Err(error) => modal.set_error(error),
                    }
                }
                Ok(_) => modal.set_result("command is not connected"),
                Err(error) => modal.set_error(error.to_string()),
            }
        }
        Key::Quit
        | Key::CtrlQ
        | Key::CtrlD
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => {}
    }
    false
}

/// Start a create without blocking the terminal event loop. The sidebar gains a
/// v1-style skeleton immediately; the worker returns the port with its result
/// so later commands still share the same daemon client state.
#[coverage(off)]
fn begin_session_create(ui: &mut WorkspaceUi, command: SessionCommand) {
    let SessionCommand::Create { name } = command else {
        return;
    };
    let Some(mut port) = ui.session_commands.take() else {
        if let Some(WorkspaceModal::Overview(modal)) = ui.modal.as_mut() {
            modal.set_error("a session command is already running");
        }
        return;
    };
    let workspace = ui.workspace.record().clone();
    let selected = ui.workspace.selected_session().cloned();
    ui.workspace.begin_pending_session(name.clone());
    ui.create_auto_closeup = Some(name.clone());
    if let Some(WorkspaceModal::Overview(modal)) = ui.modal.as_mut() {
        modal.set_result(format!("Creating session {name}…"));
    }
    let sender = ui.session_completion_sender.clone();
    std::thread::spawn(move || {
        let result = port.execute(
            &workspace,
            selected.as_ref(),
            SessionCommand::Create { name },
        );
        let _ = sender.send(SessionCommandCompletion { port, result });
    });
}

/// Apply a completed daemon result to the sidebar and the palette result band.
#[coverage(off)]
fn apply_session_result(ui: &mut WorkspaceUi, result: SessionCommandResult) {
    apply_session_projection(ui, result.sessions, result.session_ids);
    if let Some(WorkspaceModal::Overview(modal)) = ui.modal.as_mut() {
        modal.set_result(result.message);
    }
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

/// Receive completed create workers before drawing the next frame. On success,
/// replace the skeleton with the daemon snapshot and dismiss the palette; on
/// failure, retain the palette so its safe error can be corrected or retried.
#[coverage(off)]
fn drain_session_completions(ui: &mut WorkspaceUi) {
    while let Ok(completion) = ui.session_completions.try_recv() {
        ui.session_commands = Some(completion.port);
        ui.workspace.clear_pending_session();
        match completion.result {
            Ok(result) => {
                apply_session_projection(ui, result.sessions, result.session_ids);
                ui.workspace.finish_inline_session_create();
                ui.modal = None;
                if let Some(name) = ui.create_auto_closeup.take()
                    && let Some(index) = ui
                        .workspace
                        .sessions()
                        .iter()
                        .position(|session| session.name == name)
                {
                    ui.workspace.select_row(index + 1);
                    ui.enter_closeup();
                }
            }
            Err(error) => {
                ui.create_auto_closeup = None;
                ui.show_error_dialog(&error);
            }
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
            PaneLaunch::Diff { operation } => {
                let document = ui.overlay_data.diff(&ui.workspace);
                let lines = match document {
                    OverlayDocument::Ready(lines) if !lines.is_empty() => lines,
                    OverlayDocument::Ready(_) => {
                        vec![Style::new().dim().paint("No diff content available.")]
                    }
                    OverlayDocument::Unavailable(message) => {
                        vec![Style::new().dim().paint(&message)]
                    }
                };
                ui.workspace.resolve_pane(operation, lines);
            }
            PaneLaunch::Fail { operation, message } => ui.workspace.fail_pane(operation, message),
        }
    }
}

/// Apply completed launch workers before a redraw. The workspace reducer owns
/// the focus decision, using its interaction counter captured at request time.
#[coverage(off)]
fn drain_pane_completions(ui: &mut WorkspaceUi, geometry: Geometry) {
    while let Ok(completion) = ui.pane_completions.try_recv() {
        if let Some(agent) = ui.agent.as_mut() {
            agent.port = Some(completion.port);
        }
        match completion.outcome {
            PaneLaunchOutcome::Agent { operation, result } => match result {
                Ok(terminal) if pending_pane(ui, operation) => {
                    ui.workspace.complete_pane(operation, terminal.clone());
                    ui.start_terminal_session(terminal, geometry);
                }
                Err(message) if pending_pane(ui, operation) => {
                    ui.workspace.fail_pane(operation, message.clone());
                    record_agent_launch_failure(&message);
                    ui.show_error_dialog(&message);
                }
                Ok(_) | Err(_) => {}
            },
            PaneLaunchOutcome::Terminal { operation, result } => match result {
                Ok(terminal) if pending_pane(ui, operation) => {
                    ui.workspace.complete_pane(operation, terminal.clone());
                    ui.start_terminal_session(terminal, geometry);
                }
                Err(message) if pending_pane(ui, operation) => {
                    ui.workspace.fail_pane(operation, message);
                }
                Ok(_) | Err(_) => {}
            },
        }
    }
}

fn pending_pane(ui: &WorkspaceUi, operation: OperationId) -> bool {
    ui.workspace.pane().tabs().iter().any(|tab| {
        matches!(tab, crate::usecase::application::pane::PaneTab::Pending(pending)
            if pending.operation == operation)
    })
}

/// Retain a safe Agent-launch failure without letting diagnostic IO affect the
/// Closeup recovery path. Unit tests exercise the UI state transition without
/// creating files in a developer's configured data directory; `ErrorLog`
/// itself verifies append and retention behaviour in `usagi-core`.
#[cfg(not(test))]
#[coverage(off)]
fn record_agent_launch_failure(message: &str) {
    ErrorLog::record(&format!("agent launch failed: {message}"));
}

#[cfg(test)]
#[coverage(off)]
fn record_agent_launch_failure(_message: &str) {}

/// Apply the selector's checked entries one at a time through the existing
/// daemon-owned port. A checked record must still match the current projection;
/// a refreshed or removed row is skipped rather than rebound by its display
/// position or name.
#[coverage(off)]
fn submit_remove_selector(
    ui: &mut WorkspaceUi,
    entries: Vec<usagi_core::domain::session::SessionRecord>,
    force: bool,
) {
    for entry in entries {
        let current = ui
            .workspace
            .sessions()
            .iter()
            .find(|candidate| remove_modal::same_incarnation(candidate, &entry))
            .cloned();
        let Some(current) = current else {
            if let Some(WorkspaceModal::Remove(modal)) = ui.modal.as_mut() {
                modal.remove_entry(&entry);
                modal.set_feedback("skipped a session that no longer exists");
            }
            continue;
        };
        let result = ui
            .session_commands
            .as_mut()
            .expect("session port is available")
            .execute(
                ui.workspace.record(),
                Some(&current),
                SessionCommand::Remove {
                    name: current.name.clone(),
                    force,
                },
            );
        match result {
            Ok(result) => {
                let Some(sessions) = result.sessions else {
                    // A successful-looking acknowledgement without an
                    // authoritative snapshot cannot prove which incarnation
                    // survived. Keep the checked row instead of claiming a
                    // local deletion.
                    if let Some(WorkspaceModal::Remove(modal)) = ui.modal.as_mut() {
                        modal.set_feedback(result.message);
                    }
                    continue;
                };
                ui.workspace.replace_sessions(sessions);
                ui.refresh_closeup_after_session_snapshot();
                if let Some(WorkspaceModal::Remove(modal)) = ui.modal.as_mut() {
                    modal.reconcile(ui.workspace.sessions());
                }
            }
            Err(error) => {
                if let Some(WorkspaceModal::Remove(modal)) = ui.modal.as_mut() {
                    modal.set_feedback(error);
                }
            }
        }
    }
    if matches!(ui.modal.as_ref(), Some(WorkspaceModal::Remove(modal)) if modal.is_empty()) {
        ui.modal = None;
    }
}

/// The selector captures all normal and live input while open. Esc simply
/// restores the underlying Switch / Closeup surface; Enter adds no confirmation
/// step, matching v1.
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_remove_selector(ui: &mut WorkspaceUi, key: Key) -> bool {
    let submit = {
        let WorkspaceModal::Remove(modal) = ui.modal.as_mut().expect("remove modal is active")
        else {
            return false;
        };
        match key {
            Key::Up | Key::Char('k') => {
                modal.move_up();
                None
            }
            Key::Down | Key::Char('j') => {
                modal.move_down();
                None
            }
            Key::Char(' ') => {
                modal.toggle();
                None
            }
            Key::Enter => {
                let entries = modal.selected_entries();
                if entries.is_empty() {
                    modal.set_feedback("select at least one session");
                    None
                } else {
                    Some((entries, modal.force()))
                }
            }
            Key::Escape => return true,
            Key::Left
            | Key::Right
            | Key::Backspace
            | Key::Tab
            | Key::Quit
            | Key::CtrlQ
            | Key::CtrlD
            | Key::Char(_)
            | Key::Live(_)
            | Key::Click { .. }
            | Key::Pointer(_)
            | Key::Passthrough(_)
            | Key::Other => None,
        }
    };
    if let Some((entries, force)) = submit {
        submit_remove_selector(ui, entries, force);
    }
    false
}

/// Confirming an end-workspace request stops every session through the same
/// daemon-owned lifecycle operation used by the explicit remove command.
#[coverage(off)]
fn end_workspace(ui: &mut WorkspaceUi) {
    let entries = ui.workspace.sessions().to_vec();
    for entry in entries {
        let _ = ui
            .session_commands
            .as_mut()
            .expect("session port is available")
            .execute(
                ui.workspace.record(),
                Some(&entry),
                SessionCommand::Remove {
                    name: entry.name.clone(),
                    force: true,
                },
            );
    }
}

#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_quit_confirmation(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    let WorkspaceModal::Quit(modal) = ui.modal.as_mut().expect("quit modal is active") else {
        return WorkspaceStep::Stay;
    };
    match key {
        Key::Left | Key::Right | Key::Tab => {
            modal.confirmation.toggle();
            WorkspaceStep::Stay
        }
        Key::Char('y' | 'Y') => {
            modal.confirmation.select_confirm();
            confirm_quit(ui)
        }
        Key::Char('n' | 'N') | Key::Escape => {
            modal.confirmation.select_cancel();
            ui.modal = None;
            WorkspaceStep::Stay
        }
        Key::Enter => confirm_quit(ui),
        _ => WorkspaceStep::Stay,
    }
}

#[coverage(off)]
fn confirm_quit(ui: &mut WorkspaceUi) -> WorkspaceStep {
    let WorkspaceModal::Quit(modal) = ui.modal.take().expect("quit modal is active") else {
        return WorkspaceStep::Stay;
    };
    if !modal.confirmation.is_confirm_selected() {
        return WorkspaceStep::Stay;
    }
    if modal.action == QuitAction::EndWorkspace {
        end_workspace(ui);
    }
    WorkspaceStep::Quit
}

/// Input-only Overview reducer retained for modal rendering scenarios. Runtime
/// execution uses [`step_overview_command`] so session commands reach its port.
#[cfg(test)]
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_overview(modal: &mut OverviewModal, key: Key) -> bool {
    match key {
        Key::Up => {
            if !modal.recall_previous() {
                modal.select_prev();
            }
        }
        Key::Down => {
            if !modal.recall_next() {
                modal.select_next();
            }
        }
        Key::Left => modal.cursor_left(),
        Key::Right => modal.cursor_right(),
        Key::Backspace => modal.backspace(),
        Key::Tab => modal.complete_selected(),
        Key::Char(ch) => modal.insert_char(ch),
        Key::Escape => return true,
        Key::Enter => modal.record_submission(),
        Key::Quit
        | Key::CtrlQ
        | Key::CtrlD
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => {}
    }
    false
}

/// PR modal の入力処理。Enter のブラウザ起動は外部 IO port が接続されるまで no-op とする。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_pr(modal: &mut PrModal, key: Key) -> bool {
    match key {
        Key::Up | Key::Char('k') => modal.select_prev(),
        Key::Down | Key::Char('j') => modal.select_next(),
        Key::Escape => return true,
        Key::Left
        | Key::Right
        | Key::Enter
        | Key::Tab
        | Key::Backspace
        | Key::Quit
        | Key::CtrlQ
        | Key::CtrlD
        | Key::Char(_)
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => {}
    }
    false
}

/// 長文 overlay の入力処理。背景の cursor / tab は動かさない。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_text_overlay(modal: &mut TextOverlay, key: Key) -> bool {
    match key {
        Key::Up | Key::Char('k') => modal.scroll_up(),
        Key::Down | Key::Char('j') => modal.scroll_down(),
        Key::Escape => return true,
        Key::Left
        | Key::Right
        | Key::Enter
        | Key::Tab
        | Key::Backspace
        | Key::Quit
        | Key::CtrlQ
        | Key::CtrlD
        | Key::Char(_)
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => {}
    }
    false
}

/// Switch のキー処理。session 選択と preview tab の移動を行い、Enter / `t` で
/// 選択行の Closeup action menu へ入る。基底の workspace は back stack の終端なので、
/// Esc はここから抜けず no-op とする。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_switch(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    if ui.workspace.creating_session_inline() {
        match key {
            Key::Escape => ui.workspace.cancel_inline_session_create(),
            Key::Enter => {
                if let Some(name) = ui.workspace.inline_create_name() {
                    begin_session_create(ui, SessionCommand::Create { name });
                }
            }
            Key::Backspace => ui.workspace.inline_create_backspace(),
            Key::Left => ui.workspace.inline_create_move(false),
            Key::Right => ui.workspace.inline_create_move(true),
            Key::CtrlQ => ui.open_quit_confirmation(QuitAction::EndWorkspace),
            Key::Char(character) if !character.is_control() => {
                ui.workspace.inline_create_insert(character);
            }
            Key::Up
            | Key::Down
            | Key::Tab
            | Key::Quit
            | Key::CtrlD
            | Key::Char(_)
            | Key::Live(_)
            | Key::Click { .. }
            | Key::Pointer(_)
            | Key::Passthrough(_)
            | Key::Other => {}
        }
        return WorkspaceStep::Stay;
    }
    match key {
        Key::Up | Key::Char('k') => ui.workspace.select_prev(),
        Key::Down | Key::Char('j') => ui.workspace.select_next(),
        Key::Left | Key::Char('h') => ui.workspace.tab_prev(),
        Key::Right | Key::Char('l') => ui.workspace.tab_next(),
        Key::Char(character) if ui.workspace.new_session_selected() && !character.is_control() => {
            ui.workspace.begin_inline_session_create(Some(character));
        }
        Key::Enter | Key::Char('t') if ui.workspace.new_session_selected() => {
            ui.workspace.begin_inline_session_create(None);
        }
        Key::Enter | Key::Char('t') => ui.enter_closeup(),
        Key::Char('\u{1}') => {
            ui.workspace.select_new_session();
            ui.workspace.begin_inline_session_create(None);
        }
        Key::Char('c') => ui.workspace.begin_inline_session_create(None),
        Key::Char(':') => ui.open_overview(),
        Key::Char('p') => ui.open_prs(),
        Key::Char('v') => ui.open_preview(),
        Key::Char('d') => ui.open_diff(),
        Key::Char('n') => ui.open_text(),
        Key::CtrlQ => ui.open_quit_confirmation(QuitAction::EndWorkspace),
        Key::Char('q') => return WorkspaceStep::Quit,
        // `Ctrl-O a` は Switch からも選択 target の Closeup action を開く。
        // ほかの live prefix は Closeup-scoped なので Switch では no-op。
        Key::Live(LiveTerminalAction::OpenCloseupModal) => ui.open_closeup_action(),
        Key::Escape
        | Key::Backspace
        | Key::Tab
        | Key::Quit
        | Key::CtrlD
        | Key::Char(_)
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => {}
    }
    WorkspaceStep::Stay
}

/// Closeup のキー処理。tab の有無で入力の所有者を切り替える:
///
/// - live-terminal prefix（`Ctrl-O` leader）で解決した [`Key::Live`] は、modal の表示に
///   かかわらず [`LiveInputClassifier`] 契約として [`apply_live_action`] が処理する。
/// - action modal が前面のとき（tab 無し、または `Ctrl-O a` で forced）は action menu を
///   操作する。Action mode は上下で選んだ command、Prompt mode は入力した command を
///   Enter で registry 経由に実行する。
/// - tab が前面のとき（tab あり・非 forced）は tab を操作し、menu には触れない。
///
/// Esc は Workspace の mode を変えない。forced action modal が前面なら、それだけを閉じる。
///
/// [`LiveInputClassifier`]: crate::usecase::terminal_input::LiveInputClassifier
#[coverage(off)]
fn step_closeup(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    if let Key::Live(action) = key {
        return apply_live_action(ui, action);
    }
    if !ui.closeup_modal_visible() {
        return step_closeup_tabs(ui, key);
    }
    if ui.closeup.selection_mode() == ModalSelectionMode::Prompt {
        match key {
            Key::Left => ui.closeup.cursor_left(),
            Key::Right => ui.closeup.cursor_right(),
            Key::Backspace => ui.closeup.backspace(),
            Key::Char('q') | Key::Quit | Key::CtrlQ => return WorkspaceStep::Quit,
            Key::Char(ch) => ui.closeup.insert_char(ch),
            Key::Escape => close_closeup_modal(ui),
            Key::Enter => {
                let input = ui.closeup.submission();
                execute_closeup_command(ui, &input);
            }
            Key::Tab => ui.closeup.complete_selected(),
            Key::Up
            | Key::Down
            | Key::CtrlD
            | Key::Live(_)
            | Key::Click { .. }
            | Key::Pointer(_)
            | Key::Passthrough(_)
            | Key::Other => {}
        }
        return WorkspaceStep::Stay;
    }
    step_closeup_menu(ui, key)
}

/// action modal が前面のときの menu 操作。Enter は選択 action で pane を開き、開いた後は
/// forced modal を倒して新しい tab を前面へ出す。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_closeup_menu(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    match key {
        Key::Up => ui.closeup.select_prev(),
        Key::Down => ui.closeup.select_next(),
        Key::Left => {
            ui.closeup.collapse();
        }
        Key::Right => ui.closeup.expand_selected(),
        Key::Char(':') => ui.open_overview(),
        Key::Char('p') => ui.open_prs(),
        Key::Escape => close_closeup_modal(ui),
        Key::Quit | Key::CtrlQ | Key::Char('q') => return WorkspaceStep::Quit,
        Key::Enter => {
            let input = ui.closeup.submission();
            execute_closeup_command(ui, &input);
        }
        Key::Backspace => ui.closeup.backspace(),
        Key::Char(ch) => ui.closeup.insert_char(ch),
        Key::Tab => ui.closeup.complete_selected(),
        Key::CtrlD
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => {}
    }
    WorkspaceStep::Stay
}

/// tab が前面のときの操作。左右で tab を巡回し、`x` で閉じる。overlay / quit は共通。
/// action menu は前面に無いので上下・Enter は無視する。
#[coverage(off)]
#[allow(clippy::needless_pass_by_value)]
fn step_closeup_tabs(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    match key {
        Key::Left | Key::Char('h') => ui.workspace.tab_prev(),
        Key::Right | Key::Char('l') => ui.workspace.tab_next(),
        Key::Char('x') => ui.close_focused_pane(),
        Key::Char(':') => ui.open_overview(),
        Key::Char('p') => ui.open_prs(),
        Key::Char('v') => ui.open_preview(),
        Key::Char('d') => ui.open_diff(),
        Key::Char('n') => ui.open_text(),
        Key::Quit | Key::CtrlQ | Key::Char('q') => return WorkspaceStep::Quit,
        Key::Escape
        | Key::Up
        | Key::Down
        | Key::Enter
        | Key::Backspace
        | Key::Tab
        | Key::CtrlD
        | Key::Char(_)
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Other => {}
    }
    WorkspaceStep::Stay
}

/// live-terminal prefix（`Ctrl-O` leader）で解決したアクションを Closeup へ適用する。
/// [`LiveInputClassifier`] が契約の単一情報源で、ここはその action を view 操作へ写すだけ。
///
/// [`LiveInputClassifier`]: crate::usecase::terminal_input::LiveInputClassifier
#[coverage(off)]
fn apply_live_action(ui: &mut WorkspaceUi, action: LiveTerminalAction) -> WorkspaceStep {
    match action {
        LiveTerminalAction::Switch => {
            ui.enter_switch();
        }
        LiveTerminalAction::OpenCloseupModal => ui.open_closeup_action(),
        LiveTerminalAction::NextTab => ui.workspace.tab_next(),
        LiveTerminalAction::PreviousTab => ui.workspace.tab_prev(),
        LiveTerminalAction::Agent => open_pane_from_menu(ui, PaneKind::Agent),
        LiveTerminalAction::CloseTab => ui.close_focused_pane(),
        LiveTerminalAction::QuitConfirmation => ui.open_quit_confirmation(QuitAction::CloseTui),
        LiveTerminalAction::ScrollUp => ui.workspace.terminal_scroll_up(),
        LiveTerminalAction::ScrollDown => ui.workspace.terminal_scroll_down(),
        LiveTerminalAction::CopyTerminalSelection => ui.queue_terminal_copy(),
    }
    WorkspaceStep::Stay
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

#[coverage(off)]
fn handle_terminal_pointer(ui: &mut WorkspaceUi, column: u16, row: u16, kind: Option<PointerKind>) {
    let auto_scroll = workspace::terminal_auto_scroll_direction_at(
        ui.terminal_size.0,
        ui.terminal_size.1,
        &ui.workspace,
        column,
        row,
    );
    let Some(point) = workspace::terminal_point_at(
        ui.terminal_size.0,
        ui.terminal_size.1,
        &ui.workspace,
        column,
        row,
    )
    .or_else(|| {
        auto_scroll.and_then(|older| {
            workspace::terminal_edge_point(
                ui.terminal_size.0,
                ui.terminal_size.1,
                &ui.workspace,
                older,
            )
        })
    }) else {
        return;
    };
    match kind {
        // A press alone is not a selection. Keep its cell as a potential
        // anchor, and create the selection only after the pointer actually
        // moves; this preserves ordinary terminal clicks.
        None => {
            // Match native text selection: a fresh press immediately clears an
            // old range; only a subsequent drag creates the next one.
            ui.terminal_selection = None;
            ui.pending_clipboard_text = None;
            ui.dragging_terminal_selection = false;
            ui.auto_scroll_terminal_selection = None;
            ui.workspace.set_terminal_feedback(None);
            ui.pending_terminal_pointer = Some(point);
        }
        Some(PointerKind::Drag) => {
            if let Some(anchor) = ui.pending_terminal_pointer.take() {
                ui.terminal_selection_at(anchor);
                ui.dragging_terminal_selection = true;
            }
            if ui.dragging_terminal_selection {
                ui.extend_terminal_selection(point);
                ui.auto_scroll_terminal_selection = auto_scroll;
            }
        }
        Some(PointerKind::Up) => {
            ui.pending_terminal_pointer = None;
            ui.auto_scroll_terminal_selection = None;
            if ui.dragging_terminal_selection {
                ui.extend_terminal_selection(point);
                ui.dragging_terminal_selection = false;
                // macOS terminal emulators often reserve Cmd-C before it can
                // reach crossterm. Copy on release, as v1 did, so this custom
                // selection is reliably placed on the system clipboard.
                ui.queue_terminal_copy();
            }
        }
    }
}

/// Open a pane and hide the (possibly forced) action modal so the new tab is front.
#[coverage(off)]
fn open_pane_from_menu(ui: &mut WorkspaceUi, kind: PaneKind) {
    open_pane(ui, kind, None);
}

// This adapter only projects an injected daemon result into the view.  Its
// command/effect semantics are covered by `application::agent_runtime` with a
// fake port; terminal presentation keeps the loop and the projection together.
#[coverage(off)]
fn open_pane(ui: &mut WorkspaceUi, kind: PaneKind, profile: Option<AgentProfileId>) {
    let operation = ui.workspace.open_pane(kind);
    if kind == PaneKind::Agent || kind == PaneKind::Terminal {
        let selected = ui.workspace.selected().checked_sub(1);
        let launch = ui.agent.as_ref().and_then(|agent| {
            selected
                .and_then(|index| agent.sessions.get(index).copied())
                .map(|session| {
                    if kind == PaneKind::Agent {
                        let profile = profile.or_else(|| Some(agent.default_profile.clone()));
                        PaneLaunch::Agent {
                            operation,
                            workspace: agent.workspace,
                            session,
                            profile,
                        }
                    } else {
                        PaneLaunch::Terminal {
                            operation,
                            workspace: agent.workspace,
                            session,
                        }
                    }
                })
        });
        ui.pane_launches.push(launch.unwrap_or(PaneLaunch::Fail {
            operation,
            message: "select an active session to open a terminal".to_owned(),
        }));
    }
    ui.closeup_action_forced = false;
}

/// Esc / close on the action modal clears only a forced request. The Closeup mode
/// itself remains active, including when it has no tabs.
#[coverage(off)]
fn close_closeup_modal(ui: &mut WorkspaceUi) {
    if ui.closeup_action_forced {
        ui.closeup_action_forced = false;
    }
}

/// Execute the Closeup command shared by Action and Prompt interaction modes.
///
/// The closeup registry remains the single source of command names; commands
/// without a connected runtime effect intentionally keep the existing no-op
/// behaviour in both modes. Opening a pane also drops the forced action modal so
/// the freshly opened tab is front (see [`open_pane_from_menu`]).
#[coverage(off)]
fn execute_closeup_command(ui: &mut WorkspaceUi, input: &str) {
    match closeup::interpret(input) {
        Ok(closeup::Command::Agent { arguments }) => {
            let profile = match arguments.split_whitespace().next() {
                None => None,
                Some(value) => {
                    if let Ok(profile) = AgentProfileId::new(value) {
                        Some(profile)
                    } else {
                        let operation = ui.workspace.open_pane(PaneKind::Agent);
                        ui.workspace
                            .fail_pane(operation, "invalid agent profile".to_owned());
                        return;
                    }
                }
            };
            open_pane(ui, PaneKind::Agent, profile);
        }
        Ok(closeup::Command::Terminal { .. }) => open_pane_from_menu(ui, PaneKind::Terminal),
        Ok(closeup::Command::Close { arguments }) => {
            if let Ok(force) = parse_close_force(&arguments) {
                ui.open_remove_selector(force);
            }
        }
        Ok(closeup::Command::Diff { .. }) => ui.open_diff(),
        Err(_) => {}
    }
}

/// Closeup shares the remove selector and accepts only its force option; a
/// target would bypass the snapshot-backed checklist.
#[coverage(off)]
fn parse_close_force(arguments: &str) -> Result<bool, &'static str> {
    let request = crate::usecase::session_remove::parse(arguments)?;
    request
        .target
        .is_none()
        .then_some(request.force)
        .ok_or("close does not accept a session target")
}

/// Workspace 画面のキー処理。終了要求は確認 modal を開き、それ以外は最前面 modal、現在 mode の
/// 順に dispatch する。これにより背面の session / tab が modal 操作で動かない。
#[coverage(off)]
fn step_workspace(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    if key == Key::Other {
        // The runtime maps ticks and backend wakeups to `Other`; they are not
        // user input and must not cancel a pending create's safe landing.
        ui.advance_terminal_auto_scroll();
        return WorkspaceStep::Stay;
    }
    // Keep pane launches on the same no-later-interaction contract as session
    // creation. The launch key itself is counted before its acceptance marker;
    // only a subsequent user action cancels the completion focus.
    ui.workspace.record_interaction();
    // The create-triggering key arrives before `begin_session_create` sets this
    // marker. Every subsequent user key is an explicit choice to keep
    // navigating, so a late completion must not steal focus into Closeup.
    if ui.workspace.pending_session().is_some() {
        ui.create_auto_closeup = None;
    }
    // A failure dialog is acknowledgement-only: no command, including quit,
    // should leak through to the workspace behind it.
    if matches!(ui.modal, Some(WorkspaceModal::Error(_))) {
        ui.modal = None;
        return WorkspaceStep::Stay;
    }
    match key {
        Key::Click { column, row } => {
            handle_terminal_pointer(ui, column, row, None);
            return WorkspaceStep::Stay;
        }
        Key::Pointer(PointerEvent { kind, column, row }) => {
            handle_terminal_pointer(ui, column, row, Some(kind));
            return WorkspaceStep::Stay;
        }
        _ => {}
    }
    // A focused live terminal owns ordinary keys (letters, Enter, arrows) so
    // shell input reaches the daemon PTY.  Reserved chords and the global quit
    // keys fall through, and any open modal keeps priority.
    if ui.modal.is_none() && !ui.closeup_modal_visible() && ui.forward_terminal_input(&key) {
        return WorkspaceStep::Stay;
    }

    if key == Key::CtrlQ {
        ui.open_quit_confirmation(QuitAction::EndWorkspace);
        return WorkspaceStep::Stay;
    }

    if key == Key::Char('q') {
        ui.open_quit_confirmation(QuitAction::CloseTui);
        return WorkspaceStep::Stay;
    }

    // Switch has a single explicit exit chord: Ctrl-Q.  Ctrl-C must not
    // accidentally close the workspace while the session selector owns input.
    if key == Key::Quit && ui.workspace.mode() != Mode::Switch {
        return WorkspaceStep::Quit;
    }

    if let Some(modal) = &mut ui.modal {
        let close = match modal {
            WorkspaceModal::Overview(_) => step_overview_command(ui, key),
            WorkspaceModal::Pr(modal) => step_pr(modal, key),
            WorkspaceModal::Remove(_) => step_remove_selector(ui, key),
            WorkspaceModal::Text(modal) => step_text_overlay(modal, key),
            WorkspaceModal::Error(_) => true,
            WorkspaceModal::Quit(_) => return step_quit_confirmation(ui, key),
        };
        if close {
            ui.modal = None;
        }
        return WorkspaceStep::Stay;
    }

    match ui.workspace.mode() {
        Mode::Switch => step_switch(ui, key),
        Mode::Closeup => step_closeup(ui, key),
    }
}

/// Workspace と、その時点で最前面にある modal を 1 フレームへ合成する。
#[coverage(off)]
fn render_workspace(height: usize, width: usize, ui: &WorkspaceUi) -> Vec<String> {
    let base =
        workspace::render_with_skeleton_frame(height, width, &ui.workspace, ui.skeleton_frame);
    match &ui.modal {
        Some(WorkspaceModal::Overview(modal)) => {
            overview_modal::render_over(height, width, &base, modal)
        }
        Some(WorkspaceModal::Pr(modal)) => pr_modal::render_over(height, width, &base, modal),
        Some(WorkspaceModal::Remove(modal)) => {
            remove_modal::render_over(height, width, &base, modal)
        }
        Some(WorkspaceModal::Text(modal)) => text_overlay::render_over(height, width, &base, modal),
        Some(WorkspaceModal::Error(modal)) => {
            text_overlay::render_over(height, width, &base, modal)
        }
        Some(WorkspaceModal::Quit(modal)) => render_quit_confirmation(height, width, &base, *modal),
        None if ui.closeup_modal_visible() => {
            closeup_modal::render_over(height, width, &base, &ui.closeup)
        }
        None => base,
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

#[coverage(off)]
fn render_quit_confirmation(
    height: usize,
    width: usize,
    base: &[String],
    modal_state: QuitModal,
) -> Vec<String> {
    let (title, heading, message, confirm_role) = match modal_state.action {
        QuitAction::CloseTui => (
            Style::new().fg(Color::White).bold().paint("Close TUI"),
            Style::new()
                .fg(Color::White)
                .bold()
                .paint("Close this TUI?"),
            "Daemon sessions keep running.",
            Role::Success,
        ),
        QuitAction::EndWorkspace => (
            Style::new().fg(Color::White).bold().paint("End workspace"),
            Style::new()
                .fg(Color::White)
                .bold()
                .paint("End this workspace?"),
            "All live sessions will be stopped.",
            Role::Danger,
        ),
    };
    modal::render_confirmation_over(
        height,
        width,
        base,
        modal_state.confirmation,
        ConfirmationView {
            title: &title,
            inner_width: 52,
            heading,
            message,
            confirm_role,
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

/// 1 つの Workspace snapshot を、終了または Esc まで同じ Terminal 上で駆動する。
#[coverage(off)]
fn drive_workspace(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
) -> io::Result<WorkspaceStep> {
    drive_workspace_with_ports(
        term,
        snapshot,
        Box::new(SnapshotOverlayData),
        Box::new(UnavailableSessionCommandPort),
    )
}

/// `overlay_data` を注入して 1 つの Workspace snapshot を駆動する。
///
/// diff / PR の backend fetch は実装しない。この seam に安全な projection を実装して
/// 注入することで、表示層を外部 IO や生エラーから分離する。
///
/// # Errors
///
/// 端末への描画またはキー読み取りに失敗した場合、そのエラーを返す。
#[coverage(off)]
pub fn run_workspace_with_overlay_data(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    overlay_data: Box<dyn OverlayDataPort>,
) -> io::Result<Exit> {
    drive_workspace_with_overlay_data(term, snapshot, overlay_data).map(|_| Exit::Quit)
}

#[coverage(off)]
fn drive_workspace_with_overlay_data(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    overlay_data: Box<dyn OverlayDataPort>,
) -> io::Result<WorkspaceStep> {
    drive_workspace_with_ports(
        term,
        snapshot,
        overlay_data,
        Box::new(UnavailableSessionCommandPort),
    )
}

#[coverage(off)]
fn drive_workspace_with_ports(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    overlay_data: Box<dyn OverlayDataPort>,
    session_commands: Box<dyn SessionCommandPort>,
) -> io::Result<WorkspaceStep> {
    drive_workspace_with_ports_and_selection_mode(
        term,
        snapshot,
        overlay_data,
        session_commands,
        ModalSelectionMode::Action,
    )
}

#[coverage(off)]
fn drive_workspace_with_ports_and_selection_mode(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    overlay_data: Box<dyn OverlayDataPort>,
    session_commands: Box<dyn SessionCommandPort>,
    modal_selection_mode: ModalSelectionMode,
) -> io::Result<WorkspaceStep> {
    let workspace = WorkspaceView::new(snapshot.workspace, snapshot.state);
    let mut ui = WorkspaceUi::with_ports_and_selection_mode(
        workspace,
        overlay_data,
        session_commands,
        modal_selection_mode,
    );
    let mut previous_sidebar_click = None;
    loop {
        drain_session_completions(&mut ui);
        refresh_metrics(&mut ui);
        let (height, width) = term.size()?;
        ui.set_terminal_size(height, width);
        drain_pane_completions(&mut ui, terminal_geometry(height, width));
        term.draw(&render_workspace(height, width, &ui))?;
        drain_pane_launches(&mut ui, terminal_geometry(height, width));
        let key = term.read_key()?;
        ui.skeleton_frame = ui.skeleton_frame.wrapping_add(1);
        if handle_sidebar_click(&mut ui, height, width, &key, &mut previous_sidebar_click) {
            continue;
        }
        match step_workspace(&mut ui, key) {
            WorkspaceStep::Stay => {}
            WorkspaceStep::Quit => return Ok(WorkspaceStep::Quit),
        }
        if let Some(text) = ui.take_terminal_copy() {
            ui.workspace
                .set_terminal_feedback(Some(match term.copy_text(&text) {
                    Ok(()) => "terminal selection copied".to_owned(),
                    Err(error) => format!("clipboard failed: {error}"),
                }));
        }
    }
}

/// Workspace を起点にした公開 runtime。direct `usagi open <path>` は合成側で [`WorkspaceLoader`]
/// を一度呼び、その snapshot をこの関数へ渡す。基底の Switch で Esc を押しても workspace
/// からは抜けず、終了には `q`（TUI を閉じる）または Ctrl-Q（workspace を終了）を使う。
///
/// # Errors
///
/// 端末への描画またはキー読み取りに失敗した場合、そのエラーを返す。
#[coverage(off)]
pub fn run_workspace(term: &mut dyn Terminal, snapshot: WorkspaceSnapshot) -> io::Result<Exit> {
    drive_workspace(term, snapshot).map(|_| Exit::Quit)
}

/// Run a Workspace UI whose Overview session commands use the injected daemon
/// lifecycle port.
///
/// # Errors
///
/// Returns terminal IO failures from the interactive loop.
#[coverage(off)]
pub fn run_workspace_with_session_port(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
) -> io::Result<Exit> {
    run_workspace_with_session_port_and_selection_mode(
        term,
        snapshot,
        session_commands,
        ModalSelectionMode::Action,
    )
}

/// Run a Workspace UI using the saved global modal interaction mode.
///
/// The composition root reads the setting once before opening a workspace, so
/// a Config save affects the next workspace entry without changing the active
/// modal or selection of an already-running workspace.
///
/// # Errors
///
/// Returns terminal IO failures from the interactive loop.
#[coverage(off)]
pub fn run_workspace_with_session_port_and_selection_mode(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
    modal_selection_mode: ModalSelectionMode,
) -> io::Result<Exit> {
    drive_workspace_with_ports_and_selection_mode(
        term,
        snapshot,
        Box::new(SnapshotOverlayData),
        session_commands,
        modal_selection_mode,
    )
    .map(|_| Exit::Quit)
}

/// Run a workspace with the daemon-authoritative Agent launch boundary.
///
/// # Errors
///
/// Returns terminal IO failures from the interactive loop.
#[coverage(off)]
pub fn run_workspace_with_agent_port_and_selection_mode(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
    modal_selection_mode: ModalSelectionMode,
    default_model: DefaultModel,
    agent_port: Box<dyn AgentCommandPort>,
    metrics_port: Box<dyn MetricsPort>,
) -> io::Result<Exit> {
    drive_workspace_with_agent_port_and_selection_mode(
        term,
        snapshot,
        session_commands,
        modal_selection_mode,
        default_model,
        agent_port,
        metrics_port,
    )
    .map(|_| Exit::Quit)
}

// This is the real terminal composition loop.  Agent launch dispatch itself is
// covered through the injected-port tests below; exercising the loop requires
// terminal IO and belongs to the composition root.
#[coverage(off)]
fn drive_workspace_with_agent_port_and_selection_mode(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
    modal_selection_mode: ModalSelectionMode,
    default_model: DefaultModel,
    agent_port: Box<dyn AgentCommandPort>,
    metrics_port: Box<dyn MetricsPort>,
) -> io::Result<WorkspaceStep> {
    let workspace_id = snapshot.workspace_id;
    let session_ids = snapshot.session_ids.clone();
    let workspace = WorkspaceView::with_runtime_ids(
        snapshot.workspace,
        snapshot.state,
        workspace_id,
        session_ids.clone(),
    );
    let mut ui = WorkspaceUi::with_ports_and_selection_mode(
        workspace,
        Box::new(SnapshotOverlayData),
        session_commands,
        modal_selection_mode,
    )
    .with_agent_context(workspace_id, session_ids, agent_port, default_model)
    .with_metrics_port(metrics_port);
    let mut previous_sidebar_click = None;
    loop {
        drain_session_completions(&mut ui);
        refresh_metrics(&mut ui);
        let (height, width) = term.size()?;
        ui.set_terminal_size(height, width);
        drain_pane_completions(&mut ui, terminal_geometry(height, width));
        ui.resize_terminals(terminal_geometry(height, width));
        ui.refresh_terminal();
        term.draw(&render_workspace(height, width, &ui))?;
        drain_pane_launches(&mut ui, terminal_geometry(height, width));
        let key = term.read_key()?;
        ui.skeleton_frame = ui.skeleton_frame.wrapping_add(1);
        if handle_sidebar_click(&mut ui, height, width, &key, &mut previous_sidebar_click) {
            continue;
        }
        if step_workspace(&mut ui, key) == WorkspaceStep::Quit {
            return Ok(WorkspaceStep::Quit);
        }
        if let Some(text) = ui.take_terminal_copy() {
            ui.workspace
                .set_terminal_feedback(Some(match term.copy_text(&text) {
                    Ok(()) => "terminal selection copied".to_owned(),
                    Err(error) => format!("clipboard failed: {error}"),
                }));
        }
    }
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
                    let workspace_step = if let Some(factory) = agent_commands.as_deref_mut() {
                        drive_workspace_with_agent_port_and_selection_mode(
                            term,
                            snapshot,
                            session_commands.create(),
                            config_form.global_modal_selection_mode(),
                            config_form.global_default_model(),
                            factory.create(),
                            metrics.as_deref_mut().map_or_else(
                                || -> Box<dyn MetricsPort> { Box::new(NoMetrics) },
                                |factory| factory.create(),
                            ),
                        )?
                    } else {
                        drive_workspace_with_ports_and_selection_mode(
                            term,
                            snapshot,
                            Box::new(SnapshotOverlayData),
                            session_commands.create(),
                            config_form.global_modal_selection_mode(),
                        )?
                    };
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
                        let workspace_step = if let Some(factory) = agent_commands.as_deref_mut() {
                            drive_workspace_with_agent_port_and_selection_mode(
                                term,
                                snapshot,
                                session_commands.create(),
                                config_form.global_modal_selection_mode(),
                                config_form.global_default_model(),
                                factory.create(),
                                metrics.as_deref_mut().map_or_else(
                                    || -> Box<dyn MetricsPort> { Box::new(NoMetrics) },
                                    |factory| factory.create(),
                                ),
                            )?
                        } else {
                            drive_workspace_with_ports_and_selection_mode(
                                term,
                                snapshot,
                                Box::new(SnapshotOverlayData),
                                session_commands.create(),
                                config_form.global_modal_selection_mode(),
                            )?
                        };
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
        DefaultModel, DefaultSettingsPort, Exit, Geometry, MetricsPort, MetricsPortFactory,
        NewStep, NoMetricsFactory, OverlayDataPort, OverlayDocument, OverviewModal, PrModal,
        QuitAction, SessionCommandPort, SessionCommandPortFactory, SessionCommandResult,
        SnapshotOverlayData, Start, TerminalAttach, TerminalChunk, TerminalError,
        UnavailableSessionCommandPort, WelcomeStep, WorkspaceLoader, WorkspaceModal,
        WorkspaceSnapshot, WorkspaceStep, WorkspaceUi, app_event_from_key, drain_pane_completions,
        drain_pane_launches, drain_session_completions, execute_closeup_command,
        handle_sidebar_click, key_to_terminal_bytes, play_startup_splash, refresh_metrics,
        render_workspace, run as run_from_start, run_with_settings,
        run_with_settings_and_agent_and_metrics_port_factory_and_model_availability, run_workspace,
        run_workspace_with_overlay_data, run_workspace_with_session_port, step_config, step_new,
        step_overview, step_pr, step_workspace, terminal_geometry, welcome_action, write_banner,
    };
    use crate::presentation::views::config::AvailableAgentModels;
    use crate::presentation::views::new::{Field, Mode, New};
    use crate::presentation::views::welcome::MenuAction;
    use crate::presentation::views::workspace::{
        Mode as WorkspaceMode, Workspace as WorkspaceView,
    };
    use crate::usecase::application::controller::{AppEvent, AppKey};
    use crate::usecase::application::run as dispatch;
    use crate::usecase::application::{EntryScreen, Key, Terminal};
    use crate::usecase::overview::SessionCommand;
    use crate::usecase::terminal_input::LiveTerminalAction;
    use chrono::{DateTime, Duration, Utc};
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, mpsc};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::agent::AgentProfileId;
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::pullrequest::PrLink;
    use usagi_core::domain::recent::{Recent, UniteOverview};
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
    use usagi_core::domain::settings::ModalSelectionMode;
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

    fn strip_ansi(text: &str) -> String {
        let mut plain = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
            } else {
                plain.push(ch);
            }
        }
        plain
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

    fn finish_pane_launch(ui: &mut WorkspaceUi, geometry: Geometry) {
        drain_pane_launches(ui, geometry);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            drain_pane_completions(ui, geometry);
            if ui.agent.as_ref().is_none_or(|agent| agent.port.is_some()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("pane launch worker did not complete");
    }

    type SessionCommandCall = (String, Option<String>, SessionCommand);

    type AgentCommandCall = (WorkspaceId, SessionId, Option<AgentProfileId>);

    struct RecordingAgentPort(Arc<Mutex<Vec<AgentCommandCall>>>);

    impl AgentCommandPort for RecordingAgentPort {
        fn launch(
            &mut self,
            workspace: WorkspaceId,
            session: SessionId,
            profile: Option<AgentProfileId>,
        ) -> Result<TerminalRef, String> {
            self.0.lock().unwrap().push((workspace, session, profile));
            Err("agent launch is unavailable".to_owned())
        }
    }

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

    struct DeferredAgentPort {
        terminal: TerminalRef,
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl AgentCommandPort for DeferredAgentPort {
        fn launch(
            &mut self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _profile: Option<AgentProfileId>,
        ) -> Result<TerminalRef, String> {
            let _ = self.started.send(());
            self.release.recv().expect("test releases the agent launch");
            Ok(self.terminal.clone())
        }
    }

    struct SuccessfulTerminalPort(TerminalRef);

    impl AgentCommandPort for SuccessfulTerminalPort {
        fn launch(
            &mut self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _profile: Option<AgentProfileId>,
        ) -> Result<TerminalRef, String> {
            Err("agent launch is not expected".to_owned())
        }

        fn launch_terminal(
            &mut self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _geometry: Geometry,
        ) -> Result<TerminalRef, String> {
            Ok(self.0.clone())
        }
    }

    /// A fake daemon terminal: launch/attach succeed, one queued output chunk is
    /// returned on the first poll, and input bytes are recorded for assertions.
    /// `(subscription, input_seq, bytes)` recorded for each forwarded keystroke.
    type TerminalInputLog = Arc<Mutex<Vec<(u64, u64, Vec<u8>)>>>;

    struct StreamingTerminalPort {
        terminal: TerminalRef,
        replay: Vec<u8>,
        offset: u64,
        chunk: Option<TerminalChunk>,
        inputs: TerminalInputLog,
        detaches: Arc<Mutex<Vec<u64>>>,
        resizes: Arc<Mutex<Vec<Geometry>>>,
    }

    impl AgentCommandPort for StreamingTerminalPort {
        fn launch(
            &mut self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _profile: Option<AgentProfileId>,
        ) -> Result<TerminalRef, String> {
            Err("agent launch is not expected".to_owned())
        }
        fn launch_terminal(
            &mut self,
            _workspace: WorkspaceId,
            _session: SessionId,
            _geometry: Geometry,
        ) -> Result<TerminalRef, String> {
            Ok(self.terminal.clone())
        }
        fn attach_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _geometry: Geometry,
        ) -> Result<TerminalAttach, TerminalError> {
            Ok(TerminalAttach {
                subscription: 5,
                output_offset: self.offset,
                replay: self.replay.clone(),
                exited: false,
            })
        }
        fn resize_terminal(
            &mut self,
            _terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<(), TerminalError> {
            self.resizes.lock().unwrap().push(geometry);
            Ok(())
        }
        fn poll_terminal(
            &mut self,
            _terminal: &TerminalRef,
            _after_offset: u64,
        ) -> Result<Vec<TerminalChunk>, TerminalError> {
            // The command output only appears once the shell has received input.
            if self.inputs.lock().unwrap().is_empty() {
                return Ok(Vec::new());
            }
            Ok(self.chunk.take().into_iter().collect())
        }
        fn input_terminal(
            &mut self,
            _terminal: &TerminalRef,
            subscription: u64,
            input_seq: u64,
            bytes: &[u8],
        ) -> Result<(), TerminalError> {
            self.inputs
                .lock()
                .unwrap()
                .push((subscription, input_seq, bytes.to_vec()));
            Ok(())
        }
        fn detach_terminal(&mut self, _terminal: &TerminalRef, subscription: u64) {
            self.detaches.lock().unwrap().push(subscription);
        }
    }

    #[derive(Clone)]
    struct RecordingSessionPort(Arc<Mutex<Vec<SessionCommandCall>>>);

    impl SessionCommandPort for RecordingSessionPort {
        #[coverage(off)]
        fn execute(
            &mut self,
            workspace: &Workspace,
            selected: Option<&SessionRecord>,
            command: SessionCommand,
        ) -> Result<SessionCommandResult, String> {
            self.0.lock().unwrap().push((
                workspace.name.clone(),
                selected.map(|session| session.name.clone()),
                command,
            ));
            Ok(SessionCommandResult::message("daemon accepted"))
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

    fn snapshot_with_pr(name: &str) -> WorkspaceSnapshot {
        let mut snapshot = snapshot(name);
        let mut pr = PrLink::new(42, "https://example.com/pull/42");
        pr.title = Some("Workspace navigation".to_string());
        snapshot.state.sessions[0].prs.push(pr);
        snapshot
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

    struct FixedOverlayData;

    impl OverlayDataPort for FixedOverlayData {
        fn preview(&self, _workspace: &WorkspaceView) -> OverlayDocument {
            OverlayDocument::Ready(vec!["injected preview".to_string()])
        }

        #[coverage(off)]
        fn diff(&self, _workspace: &WorkspaceView) -> OverlayDocument {
            OverlayDocument::Unavailable("injected diff fallback".to_string())
        }

        #[coverage(off)]
        fn text(&self, _workspace: &WorkspaceView) -> OverlayDocument {
            OverlayDocument::Ready(vec!["injected text".to_string()])
        }

        #[coverage(off)]
        fn pull_requests(&self, _workspace: &WorkspaceView) -> Result<Vec<PrLink>, String> {
            Err("injected PR fallback".to_string())
        }
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
    #[coverage(off)]
    fn workspace_ui_passes_prompt_selection_to_both_command_modals() {
        let workspace = WorkspaceView::new(ws("prompt"), state("prompt"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Prompt,
        );
        ui.open_overview();
        let Some(WorkspaceModal::Overview(overview)) = ui.modal.as_ref() else {
            panic!("overview modal should open");
        };
        assert_eq!(overview.selection_mode(), ModalSelectionMode::Prompt);
        ui.resize_terminals(Geometry { cols: 80, rows: 24 });
        ui.modal = None;
        ui.enter_closeup();
        assert_eq!(ui.closeup.selection_mode(), ModalSelectionMode::Prompt);
    }

    #[test]
    fn closeup_prompt_executes_the_typed_action() {
        use crate::usecase::terminal_input::LiveTerminalAction;

        let workspace = WorkspaceView::new(ws("prompt"), state("prompt"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Prompt,
        );

        // With no tabs the prompt modal is front, so a typed command runs directly.
        ui.enter_closeup();
        for character in "agent".chars() {
            step_workspace(&mut ui, Key::Char(character));
        }
        step_workspace(&mut ui, Key::Enter);
        assert_eq!(ui.workspace.pane().tabs().len(), 1);

        // A tab is now present, so the prompt is hidden until `Ctrl-O a` forces it
        // back over the tabs; only then does the typed command run.
        ui.enter_closeup();
        assert!(!ui.closeup_modal_visible(), "prompt is hidden behind tabs");

        step_workspace(&mut ui, Key::Live(LiveTerminalAction::OpenCloseupModal));
        assert!(ui.closeup_modal_visible());
        for character in "terminal".chars() {
            step_workspace(&mut ui, Key::Char(character));
        }
        step_workspace(&mut ui, Key::Enter);
        assert_eq!(ui.workspace.pane().tabs().len(), 2);
    }

    #[test]
    fn modal_reducers_capture_edit_selection_and_close_keys() {
        let mut overview = OverviewModal::new();
        assert!(!step_overview(&mut overview, Key::Tab));
        assert_eq!(overview.input(), "config");
        assert!(!step_overview(&mut overview, Key::Enter));
        for _ in 0..6 {
            assert!(!step_overview(&mut overview, Key::Backspace));
        }
        assert!(!step_overview(&mut overview, Key::Up));
        assert_eq!(overview.input(), "config");

        let mut overview = OverviewModal::new();
        for key in [Key::Down, Key::Up, Key::Left, Key::Right] {
            assert!(!step_overview(&mut overview, key));
        }
        assert!(!step_overview(&mut overview, Key::Char('q')));
        assert_eq!(overview.input(), "q");
        for key in [Key::Backspace, Key::Enter, Key::Other, Key::Quit] {
            assert!(!step_overview(&mut overview, key));
        }
        assert!(step_overview(&mut overview, Key::Escape));

        let mut pr = PrModal::new(vec![PrLink::new(7, "https://example.com/pull/7")]);
        for key in [
            Key::Up,
            Key::Char('k'),
            Key::Down,
            Key::Char('j'),
            Key::Left,
            Key::Right,
            Key::Enter,
            Key::Backspace,
            Key::Quit,
            Key::Char('x'),
            Key::Other,
        ] {
            assert!(!step_pr(&mut pr, key));
        }
        assert!(step_pr(&mut pr, Key::Escape));
    }

    #[test]
    fn switch_pr_modal_captures_keys_without_moving_the_background() {
        let snapshot = snapshot_with_pr("switch");
        let workspace = WorkspaceView::new(snapshot.workspace, snapshot.state);
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        let selected = ui.workspace.selected();
        let tab = ui.workspace.active_tab();

        assert_eq!(step_workspace(&mut ui, Key::Char('p')), WorkspaceStep::Stay);
        assert!(matches!(ui.modal, Some(WorkspaceModal::Pr(_))));
        step_workspace(&mut ui, Key::Down);
        step_workspace(&mut ui, Key::Right);
        assert_eq!(ui.workspace.selected(), selected);
        assert_eq!(ui.workspace.active_tab(), tab);

        step_workspace(&mut ui, Key::Escape);
        assert!(ui.modal.is_none());
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);

        step_workspace(&mut ui, Key::Char(':'));
        assert!(matches!(ui.modal, Some(WorkspaceModal::Overview(_))));
        step_workspace(&mut ui, Key::Escape);
        assert!(ui.modal.is_none());
        assert_eq!(step_workspace(&mut ui, Key::Backspace), WorkspaceStep::Stay);
    }

    #[test]
    fn double_clicking_a_sidebar_session_selects_and_opens_its_closeup() {
        let snapshot = snapshot("click");
        let workspace = WorkspaceView::new(snapshot.workspace, snapshot.state);
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        let mut previous = None;
        let click = Key::Click { column: 0, row: 5 };

        assert!(handle_sidebar_click(
            &mut ui,
            24,
            100,
            &click,
            &mut previous
        ));
        assert_eq!(ui.workspace.selected(), 1);
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);

        assert!(handle_sidebar_click(
            &mut ui,
            24,
            100,
            &click,
            &mut previous
        ));
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Closeup);
        assert!(ui.closeup_modal_visible());
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
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('q'), Key::Enter]);
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

        let mut filter = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Char('b'),
            Key::Enter,
            Key::Char('q'),
            Key::Enter,
        ]);
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
            Key::Char('q'),
            Key::Enter,
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
        let keys = [
            Key::Char('o'),
            Key::Down,
            Key::Up,
            Key::Down,
            Key::Enter,
            Key::Down,
            Key::Right,
            Key::Left,
            Key::Up,
            Key::Enter,
            Key::Char('z'),
            Key::Other,
            Key::Escape,
            Key::Char('q'),
            Key::Enter,
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
                .last()
                .unwrap()
                .join("\n")
                .contains("beta-session")
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
            Key::Char('q'),
            Key::Enter,
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
                .last()
                .unwrap()
                .join("\n")
                .contains("alpha-session")
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
            FakeTerminal::with_keys(&[Key::Char('1'), Key::Escape, Key::Char('q'), Key::Enter]);
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
        let keys = [Key::Char('2'), Key::Escape, Key::Char('q'), Key::Enter];
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
        let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('q'), Key::Enter]);
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

    #[test]
    fn workspace_modes_modals_tabs_and_escape_stack_are_interactive() {
        let keys = [
            Key::Down,
            Key::Right,
            Key::Enter,
            Key::Down,
            Key::Char(':'),
            Key::Char('z'),
            Key::Left,
            Key::Backspace,
            Key::Escape,
            Key::Char('p'),
            Key::Down,
            Key::Escape,
            Key::Escape,
            Key::Quit,
        ];
        let mut term = FakeTerminal::with_keys(&keys);

        assert_eq!(
            run_workspace(&mut term, snapshot_with_pr("direct")).unwrap(),
            Exit::Quit
        );
        assert_eq!(term.frames.len(), keys.len());

        let frame = |index: usize| term.frames[index].join("\n");
        assert!(frame(0).contains("switch"));
        assert!(frame(0).contains("No tabs stirring yet. Enter starts one."));
        assert!(frame(2).contains("No tabs stirring yet. Enter starts one."));

        // Closeup modal は workspace と tab strip の上に重なり、左右移動後の tab を保つ。
        assert!(frame(3).contains("terminal"));
        assert!(frame(3).contains("direct-session"));

        // Overview が Closeup の上に重なり、文字入力は modal が先に処理される。
        assert!(frame(5).contains("workspace commands"));
        assert!(frame(6).contains("no matching command"));
        assert!(frame(6).contains("Overview"));
        assert!(frame(9).contains("terminal"));

        // PR modal も実データを表示し、閉じると同じ Closeup に戻る。
        assert!(frame(10).contains("Pull Request"));
        assert!(frame(10).contains("#42"));
        assert!(frame(12).contains("terminal"));

        // Closeup 上の Esc は mode を変えない。終了は明示的な Quit のみ。
        assert!(frame(13).contains("\u{f00e} closeup"));
        assert!(!frame(13).contains("Open terminal"));
    }

    #[test]
    fn closeup_actions_project_agent_and_terminal_tabs_into_the_runtime_frame() {
        let mut agent = FakeTerminal::with_keys(&[Key::Down, Key::Enter, Key::Enter, Key::Quit]);
        assert_eq!(
            run_workspace(&mut agent, snapshot("chrome-agent")).unwrap(),
            Exit::Quit
        );
        let mut terminal = FakeTerminal::with_keys(&[
            Key::Down,
            Key::Enter,
            Key::Down,
            Key::Down,
            Key::Down,
            Key::Enter,
            Key::Quit,
        ]);
        assert_eq!(
            run_workspace(&mut terminal, snapshot("chrome-terminal")).unwrap(),
            Exit::Quit
        );
        let agent_frames = agent
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        let terminal_frames = terminal
            .frames
            .iter()
            .map(|frame| frame.join("\n"))
            .collect::<Vec<_>>();
        assert!(
            agent_frames
                .iter()
                .any(|frame| strip_ansi(frame).contains("Agent"))
        );
        assert!(
            terminal_frames
                .iter()
                .any(|frame| strip_ansi(frame).contains("Terminal"))
        );
        assert!(
            agent_frames.iter().all(|frame| !frame.contains('▔')),
            "a pending tab is listed before completion but is not focused yet"
        );
        assert!(
            agent_frames
                .iter()
                .any(|frame| frame.contains("No tabs stirring yet. Enter starts one."))
        );
    }

    #[test]
    fn selecting_agent_in_closeup_submits_the_selected_session_to_the_agent_port() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let workspace = WorkspaceView::new(ws("closeup-agent"), state("closeup-agent"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        )
        .with_agent_context(
            workspace_id,
            vec![session_id],
            Box::new(RecordingAgentPort(calls.clone())),
            DefaultModel::OpenAi,
        );

        let _ = step_workspace(&mut ui, Key::Down);
        let _ = step_workspace(&mut ui, Key::Enter);
        let _ = step_workspace(&mut ui, Key::Enter);
        finish_pane_launch(&mut ui, Geometry { cols: 80, rows: 24 });

        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                workspace_id,
                session_id,
                Some(AgentProfileId::new("codex").expect("canonical profile ID")),
            ),]
        );
        let frame = render_workspace(40, 80, &ui).join("\n");
        assert!(frame.contains("Session operation failed"));
        assert!(frame.contains("agent launch is unavailable"));
        assert!(matches!(ui.modal, Some(WorkspaceModal::Error(_))));

        // Dismissing the error returns to the tab-less Closeup action modal.
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);
        assert!(ui.modal.is_none());
        assert!(ui.closeup_modal_visible());
    }

    #[test]
    fn selecting_agent_in_a_session_replaces_its_pending_tab_with_the_daemon_terminal() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id,
            session_id: Some(session_id),
            worktree_id: WorktreeId::new(),
        };
        let workspace = WorkspaceView::with_runtime_ids(
            ws("closeup-agent-live"),
            state("closeup-agent-live"),
            workspace_id,
            vec![session_id],
        );
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        )
        .with_agent_context(
            workspace_id,
            vec![session_id],
            Box::new(SuccessfulAgentPort(terminal.clone())),
            DefaultModel::OpenAi,
        );

        let _ = step_workspace(&mut ui, Key::Down);
        let _ = step_workspace(&mut ui, Key::Enter);
        let _ = step_workspace(&mut ui, Key::Enter);
        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Pending(pending)]
                if pending.kind == crate::usecase::application::pane::PaneKind::Agent
        ));
        finish_pane_launch(&mut ui, Geometry { cols: 80, rows: 24 });

        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Live(live)]
                if live.kind == crate::usecase::application::pane::PaneKind::Agent
                    && live.terminal == terminal
        ));
        assert!(matches!(
            ui.workspace.pane().selected(),
            crate::usecase::application::pane::PaneSelection::Tab(
                crate::usecase::application::pane::TabSelection::Live(selected)
            ) if *selected == terminal
        ));
    }

    #[test]
    fn input_while_an_agent_tab_loads_cancels_its_automatic_focus() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id,
            session_id: Some(session_id),
            worktree_id: WorktreeId::new(),
        };
        let (started_sender, started) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        let workspace = WorkspaceView::with_runtime_ids(
            ws("closeup-agent-interaction"),
            state("closeup-agent-interaction"),
            workspace_id,
            vec![session_id],
        );
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        )
        .with_agent_context(
            workspace_id,
            vec![session_id],
            Box::new(DeferredAgentPort {
                terminal: terminal.clone(),
                started: started_sender,
                release: release_receiver,
            }),
            DefaultModel::OpenAi,
        );

        let _ = step_workspace(&mut ui, Key::Down);
        let _ = step_workspace(&mut ui, Key::Enter);
        let _ = step_workspace(&mut ui, Key::Enter);
        drain_pane_launches(&mut ui, Geometry { cols: 80, rows: 24 });
        started.recv().expect("agent worker started");
        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Pending(pending)]
                if pending.kind == crate::usecase::application::pane::PaneKind::Agent
        ));

        let _ = step_workspace(&mut ui, Key::Down);
        release.send(()).expect("worker still receives completion");
        finish_pane_launch(&mut ui, Geometry { cols: 80, rows: 24 });

        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Live(live)] if live.terminal == terminal
        ));
        assert!(!matches!(
            ui.workspace.pane().selected(),
            crate::usecase::application::pane::PaneSelection::Tab(
                crate::usecase::application::pane::TabSelection::Live(selected)
            ) if *selected == terminal
        ));
    }

    #[test]
    fn closing_an_agent_tab_while_it_loads_discards_its_completion() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id,
            session_id: Some(session_id),
            worktree_id: WorktreeId::new(),
        };
        let (started_sender, started) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        let workspace = WorkspaceView::with_runtime_ids(
            ws("closeup-agent-close"),
            state("closeup-agent-close"),
            workspace_id,
            vec![session_id],
        );
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        )
        .with_agent_context(
            workspace_id,
            vec![session_id],
            Box::new(DeferredAgentPort {
                terminal,
                started: started_sender,
                release: release_receiver,
            }),
            DefaultModel::OpenAi,
        );

        let _ = step_workspace(&mut ui, Key::Down);
        let _ = step_workspace(&mut ui, Key::Enter);
        let _ = step_workspace(&mut ui, Key::Enter);
        drain_pane_launches(&mut ui, Geometry { cols: 80, rows: 24 });
        started.recv().expect("agent worker started");
        ui.workspace.tab_next();
        ui.close_focused_pane();
        release.send(()).expect("worker still receives completion");
        finish_pane_launch(&mut ui, Geometry { cols: 80, rows: 24 });

        assert!(ui.workspace.pane().tabs().is_empty());
        assert!(ui.terminals.is_empty());
    }

    #[test]
    fn selecting_terminal_in_a_session_replaces_its_pending_tab_with_the_daemon_terminal() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id,
            session_id: Some(session_id),
            worktree_id: WorktreeId::new(),
        };
        let mut port = SuccessfulTerminalPort(terminal.clone());
        assert_eq!(
            port.launch(workspace_id, session_id, None).unwrap_err(),
            "agent launch is not expected"
        );
        let workspace = WorkspaceView::with_runtime_ids(
            ws("closeup-terminal-live"),
            state("closeup-terminal-live"),
            workspace_id,
            vec![session_id],
        );
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        )
        .with_agent_context(
            workspace_id,
            vec![session_id],
            Box::new(port),
            DefaultModel::OpenAi,
        );

        let _ = step_workspace(&mut ui, Key::Down);
        let _ = step_workspace(&mut ui, Key::Enter);
        execute_closeup_command(&mut ui, "terminal");
        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Pending(pending)]
                if pending.kind == crate::usecase::application::pane::PaneKind::Terminal
        ));
        finish_pane_launch(&mut ui, Geometry { cols: 80, rows: 24 });

        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Live(live)]
                if live.kind == crate::usecase::application::pane::PaneKind::Terminal
                    && live.terminal == terminal
        ));
        assert!(matches!(
            ui.workspace.pane().selected(),
            crate::usecase::application::pane::PaneSelection::Tab(
                crate::usecase::application::pane::TabSelection::Live(selected)
            ) if *selected == terminal
        ));
    }

    #[test]
    fn closeup_diff_uses_the_pending_tab_lifecycle_and_selects_its_ready_tab() {
        let workspace = WorkspaceView::new(ws("closeup-diff"), state("closeup-diff"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));

        let _ = step_workspace(&mut ui, Key::Down);
        let _ = step_workspace(&mut ui, Key::Enter);
        execute_closeup_command(&mut ui, "diff");
        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Pending(pending)]
                if pending.kind == crate::usecase::application::pane::PaneKind::Diff
        ));
        finish_pane_launch(&mut ui, Geometry { cols: 80, rows: 24 });

        assert!(matches!(
            ui.workspace.pane().tabs(),
            [crate::usecase::application::pane::PaneTab::Ready(ready)]
                if ready.kind == crate::usecase::application::pane::PaneKind::Diff
        ));
        assert!(matches!(
            ui.workspace.pane().selected(),
            crate::usecase::application::pane::PaneSelection::Tab(
                crate::usecase::application::pane::TabSelection::Ready(_)
            )
        ));
        let frame = render_workspace(40, 80, &ui).join("\n");
        assert!(frame.contains("Diff"));
        assert!(frame.contains("Diff data is unavailable until a backend"));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // classifier-to-runtime acceptance scenario
    fn a_live_terminal_renders_daemon_output_and_forwards_keystrokes() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id,
            session_id: Some(session_id),
            worktree_id: WorktreeId::new(),
        };
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let detaches = Arc::new(Mutex::new(Vec::new()));
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let mut port = StreamingTerminalPort {
            terminal: terminal.clone(),
            replay: b"$ ".to_vec(),
            offset: 2,
            // The prompt echo plus the command output that a `ls` run produces.
            chunk: Some(TerminalChunk {
                start_offset: 2,
                end_offset: 2 + b"ls\r\na.txt\r\n".len() as u64,
                data: b"ls\r\na.txt\r\n".to_vec(),
            }),
            inputs: Arc::clone(&inputs),
            detaches: Arc::clone(&detaches),
            resizes: Arc::clone(&resizes),
        };
        // A generic terminal never launches an Agent through this port.
        assert!(port.launch(workspace_id, session_id, None).is_err());
        let workspace = WorkspaceView::with_runtime_ids(
            ws("closeup-terminal-io"),
            state("closeup-terminal-io"),
            workspace_id,
            vec![session_id],
        );
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        )
        .with_agent_context(
            workspace_id,
            vec![session_id],
            Box::new(port),
            DefaultModel::OpenAi,
        );

        // Select the session, enter Closeup, and open the daemon terminal. The
        // launch is queued, then drained to attach; the attach replay (the
        // prompt) renders without needing a poll.
        let _ = step_workspace(&mut ui, Key::Down);
        let _ = step_workspace(&mut ui, Key::Enter);
        execute_closeup_command(&mut ui, "terminal");
        finish_pane_launch(&mut ui, Geometry { cols: 80, rows: 24 });
        ui.refresh_terminal();
        assert!(render_workspace(24, 80, &ui).join("\n").contains('$'));

        ui.resize_terminals(Geometry {
            cols: 100,
            rows: 30,
        });
        assert_eq!(
            *resizes.lock().unwrap(),
            vec![
                Geometry { cols: 80, rows: 24 },
                Geometry {
                    cols: 100,
                    rows: 30
                },
            ]
        );

        // Typing forwards raw bytes exactly once with a monotonic sequence.
        let _ = step_workspace(&mut ui, Key::Char('l'));
        let _ = step_workspace(&mut ui, Key::Char('s'));
        let _ = step_workspace(&mut ui, Key::Enter);
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![
                (5, 0, b"l".to_vec()),
                (5, 1, b"s".to_vec()),
                (5, 2, b"\r".to_vec()),
            ]
        );

        // The next redraw polls the daemon and renders the command output.
        ui.refresh_terminal();
        let frame = render_workspace(24, 80, &ui).join("\n");
        assert!(frame.contains("$ ls"), "prompt echo missing: {frame}");
        assert!(frame.contains("a.txt"), "command output missing: {frame}");

        // Plain a/o/x stay terminal input. The Ctrl-O versions, however, are
        // reducer actions: a opens Closeup, o leaves the live-input owner, and
        // x detaches exactly the selected terminal before removing its tab.
        for character in ['a', 'o', 'x'] {
            let _ = step_workspace(&mut ui, Key::Char(character));
        }
        assert_eq!(
            *inputs.lock().unwrap(),
            vec![
                (5, 0, b"l".to_vec()),
                (5, 1, b"s".to_vec()),
                (5, 2, b"\r".to_vec()),
                (5, 3, b"a".to_vec()),
                (5, 4, b"o".to_vec()),
                (5, 5, b"x".to_vec()),
            ]
        );

        let _ = step_workspace(
            &mut ui,
            Key::Live(crate::usecase::terminal_input::LiveTerminalAction::OpenCloseupModal),
        );
        assert!(ui.closeup_modal_visible());
        let _ = step_workspace(
            &mut ui,
            Key::Live(crate::usecase::terminal_input::LiveTerminalAction::Switch),
        );
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);
        let _ = step_workspace(&mut ui, Key::Char('a'));
        assert_eq!(inputs.lock().unwrap().len(), 6);

        let _ = step_workspace(
            &mut ui,
            Key::Live(crate::usecase::terminal_input::LiveTerminalAction::OpenCloseupModal),
        );
        let _ = step_workspace(&mut ui, Key::Escape);
        let _ = step_workspace(
            &mut ui,
            Key::Live(crate::usecase::terminal_input::LiveTerminalAction::CloseTab),
        );
        assert!(ui.workspace.pane().tabs().is_empty());
        assert!(ui.closeup_modal_visible());
        assert_eq!(*detaches.lock().unwrap(), vec![5]);
        assert!(ui.terminals.is_empty());
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

    #[test]
    fn closeup_close_opens_the_shared_selector_and_dispatches_selected_session() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let keys = [
            Key::Down,
            Key::Enter,
            Key::Down,
            Key::Enter,
            Key::Char(' '),
            Key::Enter,
            Key::Quit,
        ];
        let mut term = FakeTerminal::with_keys(&keys);

        assert_eq!(
            run_workspace_with_session_port(
                &mut term,
                snapshot("remove-selector"),
                Box::new(RecordingSessionPort(calls.clone())),
            )
            .unwrap(),
            Exit::Quit
        );

        assert!(
            term.frames
                .iter()
                .map(|frame| frame.join("\n"))
                .any(|frame| frame.contains("Remove sessions")
                    && frame.contains("remove-selector-session"))
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [(
                "remove-selector".to_owned(),
                Some("remove-selector-session".to_owned()),
                SessionCommand::Remove {
                    name: "remove-selector-session".to_owned(),
                    force: false,
                },
            )]
        );
    }

    #[test]
    fn ctrl_q_confirms_then_stops_live_sessions_before_closing_workspace() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut term = FakeTerminal::with_keys(&[Key::CtrlQ, Key::Enter]);

        assert_eq!(
            run_workspace_with_session_port(
                &mut term,
                snapshot("end-workspace"),
                Box::new(RecordingSessionPort(calls.clone())),
            )
            .unwrap(),
            Exit::Quit
        );

        let confirmation = term.frames[1].join("\n");
        assert!(confirmation.contains("All live sessions will be stopped."));
        assert!(confirmation.contains("[ yes ]"));
        assert!(confirmation.contains("[ no  ]"));
        assert!(confirmation.contains("\u{1b}[1;31m"));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(
                "end-workspace".to_owned(),
                Some("end-workspace-session".to_owned()),
                SessionCommand::Remove {
                    name: "end-workspace-session".to_owned(),
                    force: true,
                },
            )]
        );
    }

    #[test]
    fn ctrl_q_confirmation_accepts_y_and_dismisses_with_n() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut terminal = FakeTerminal::with_keys(&[Key::CtrlQ, Key::Char('y')]);

        assert_eq!(
            run_workspace_with_session_port(
                &mut terminal,
                snapshot("confirm-yes"),
                Box::new(RecordingSessionPort(calls.clone())),
            )
            .unwrap(),
            Exit::Quit
        );
        assert_eq!(calls.lock().unwrap().len(), 1);

        let workspace = WorkspaceView::new(ws("confirm-no"), state("confirm-no"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        ui.open_quit_confirmation(QuitAction::EndWorkspace);
        assert_eq!(step_workspace(&mut ui, Key::Char('n')), WorkspaceStep::Stay);
        assert!(ui.modal.is_none());
    }

    #[test]
    fn closeup_action_modal_appears_without_tabs_and_hides_once_a_tab_opens() {
        // Req 1/3/4: Enter enters Closeup; with no tabs the action modal is the
        // front surface, and opening a tab hides it so the tab strip is front.
        let keys = [Key::Down, Key::Enter, Key::Enter, Key::Quit];
        let mut term = FakeTerminal::with_keys(&keys);
        assert_eq!(
            run_workspace(&mut term, snapshot("gate")).unwrap(),
            Exit::Quit
        );
        let frame = |index: usize| term.frames[index].join("\n");

        assert!(frame(2).contains("Run a command:"));
        assert!(strip_ansi(&frame(3)).contains("Agent"));
        assert!(!frame(3).contains("Run a command:"));
    }

    #[test]
    fn closeup_prefix_cycles_tabs_and_toggles_the_forced_action_modal() {
        // Req 2/5/6: with tabs present the Ctrl-O prefix owns the stream — `a`
        // forces the action modal, Esc drops back to the tabs, `n`/`p` cycle the
        // selected tab, and `o` returns to Switch.
        use crate::usecase::application::pane::PaneKind;
        use crate::usecase::terminal_input::LiveTerminalAction;

        let workspace = WorkspaceView::new(ws("prefix"), state("prefix"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        ui.enter_closeup();
        assert!(ui.closeup_modal_visible(), "no tabs -> modal is front");
        assert_eq!(step_workspace(&mut ui, Key::Escape), WorkspaceStep::Stay);
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Closeup);
        assert!(
            ui.closeup_modal_visible(),
            "Esc keeps the tab-less Closeup open"
        );

        ui.workspace.open_pane(PaneKind::Agent);
        ui.workspace.open_pane(PaneKind::Terminal);
        assert!(ui.workspace.has_panes());
        assert!(!ui.closeup_modal_visible(), "tabs present -> modal hidden");

        // Ctrl-O n / Ctrl-O p move the stable tab selection and restore it.
        let before = ui.workspace.pane().selected().clone();
        assert_eq!(
            step_workspace(&mut ui, Key::Live(LiveTerminalAction::NextTab)),
            WorkspaceStep::Stay
        );
        assert_ne!(ui.workspace.pane().selected(), &before);
        step_workspace(&mut ui, Key::Live(LiveTerminalAction::PreviousTab));
        assert!(matches!(
            ui.workspace.pane().selected(),
            crate::usecase::application::pane::PaneSelection::Tab(_)
        ));

        // Ctrl-O a forces the action modal over the tabs; Esc clears the force
        // and keeps the tabs (it does not leave Closeup).
        step_workspace(&mut ui, Key::Live(LiveTerminalAction::OpenCloseupModal));
        assert!(ui.closeup_modal_visible());
        assert_eq!(step_workspace(&mut ui, Key::Escape), WorkspaceStep::Stay);
        assert!(!ui.closeup_modal_visible());
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Closeup);

        // Ctrl-O o returns Closeup to Switch.
        step_workspace(&mut ui, Key::Live(LiveTerminalAction::Switch));
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);
    }

    #[test]
    fn live_mode_actions_replace_stale_closeup_state_across_switch_and_sessions() {
        use crate::usecase::terminal_input::LiveTerminalAction;

        let workspace = WorkspaceView::new(ws("mode-actions"), state("mode-actions"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);
        ui.workspace.select_next();

        // `Ctrl-O a` from Switch opens the selected target's Closeup and gives
        // its action modal the input surface immediately.
        step_workspace(&mut ui, Key::Live(LiveTerminalAction::OpenCloseupModal));
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Closeup);
        assert!(ui.closeup_modal_visible());
        assert_eq!(ui.closeup.session(), "mode-actions-session");

        // Switching back removes the forced action state instead of leaving a
        // Closeup surface over Switch.
        step_workspace(&mut ui, Key::Live(LiveTerminalAction::Switch));
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);
        assert!(!ui.closeup_modal_visible());
    }

    #[test]
    fn closeup_prefix_quit_and_passthrough_keys_are_preserved() {
        // A live prefix `q` opens the same TUI-close confirmation as `q`.
        use crate::usecase::terminal_input::LiveTerminalAction;

        let workspace = WorkspaceView::new(ws("prefix-quit"), state("prefix-quit"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        ui.enter_closeup();
        assert_eq!(
            step_workspace(&mut ui, Key::Live(LiveTerminalAction::QuitConfirmation)),
            WorkspaceStep::Stay
        );
        assert!(matches!(ui.modal, Some(WorkspaceModal::Quit(_))));
    }

    #[test]
    fn switch_ignores_ctrl_c_but_ctrl_q_opens_workspace_exit_confirmation() {
        let workspace = WorkspaceView::new(ws("switch-quit"), state("switch-quit"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));

        assert_eq!(step_workspace(&mut ui, Key::Quit), WorkspaceStep::Stay);
        assert!(ui.modal.is_none());
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);

        assert_eq!(step_workspace(&mut ui, Key::CtrlQ), WorkspaceStep::Stay);
        assert!(matches!(ui.modal, Some(WorkspaceModal::Quit(_))));
    }

    #[test]
    fn session_error_dialog_wraps_the_reason_and_closes_on_any_key() {
        let workspace = WorkspaceView::new(ws("error-dialog"), state("error-dialog"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        ui.show_error_dialog(
            "cannot create session \"aa\": branch usagi/aa already exists; choose a different name",
        );

        let frame = render_workspace(40, 80, &ui).join("\n");
        assert!(frame.contains("Session operation failed"));
        assert!(frame.contains('\u{f06a}'));
        assert!(frame.contains("\u{1b}[1;31m"));
        assert!(frame.contains("Press any key to close"));
        assert!(matches!(ui.modal, Some(WorkspaceModal::Error(_))));

        // `q` must acknowledge this dialog, not open the global quit modal.
        assert_eq!(step_workspace(&mut ui, Key::Char('q')), WorkspaceStep::Stay);
        assert!(ui.modal.is_none());
    }

    #[test]
    fn overview_session_command_uses_the_injected_daemon_port() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let port = RecordingSessionPort(calls.clone());
        let mut keys = vec![Key::Char(':')];
        keys.extend("session list".chars().map(Key::Char));
        keys.extend([Key::Enter, Key::Char('q'), Key::Enter]);
        let mut term = FakeTerminal::with_keys(&keys);

        assert_eq!(
            run_workspace_with_session_port(&mut term, snapshot("runner"), Box::new(port)).unwrap(),
            Exit::Quit
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("runner".to_owned(), None, SessionCommand::List)]
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("daemon accepted"))
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
        let keys = [Key::Char('o'), Key::Enter, Key::Char('q'), Key::Enter];
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
        let keys = [Key::Char('o'), Key::Enter, Key::Char('q'), Key::Enter];
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
        let keys = [Key::Char('1'), Key::Char('q'), Key::Enter];
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

    /// Overview の `session create` が注入 port へ届き、返された daemon snapshot が
    /// sidebar の session 行へ反映されることを固定する。create は worker thread で走る
    /// ため、submit 直後の pending skeleton を同期的に確認し、その後 completion を drain して
    /// 反映を検証する（thread の完了を待つので競合しない）。
    #[test]
    fn session_create_reaches_the_port_and_snapshot_reflects_in_the_sidebar() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(SnapshotSessionPort(calls.clone())),
            ModalSelectionMode::Action,
        );
        ui.open_overview();
        for ch in "session create review".chars() {
            assert_eq!(step_workspace(&mut ui, Key::Char(ch)), WorkspaceStep::Stay);
        }
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);

        // The create runs on a worker thread; the skeleton appears synchronously.
        assert_eq!(ui.workspace.pending_session(), Some("review"));

        // The runtime emits `Other` for ticks and backend wakeups. It is not
        // user input and must not cancel the completion's safe landing.
        assert_eq!(step_workspace(&mut ui, Key::Other), WorkspaceStep::Stay);

        // Wait for the worker to finish, then drain its completion.
        while ui.workspace.pending_session().is_some() {
            drain_session_completions(&mut ui);
            std::thread::yield_now();
        }

        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                "alpha".to_owned(),
                None,
                SessionCommand::Create {
                    name: "review".to_owned()
                }
            )]
        );
        // The daemon snapshot returned by the port replaces the sidebar rows.
        assert!(render_workspace(40, 80, &ui).join("\n").contains("review"));

        // Without intervening input, completion focuses the created session and
        // opens its Closeup. The pane map is initialized before that selection.
        assert_eq!(
            ui.workspace
                .selected_session()
                .map(|session| session.name.as_str()),
            Some("review")
        );
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Closeup);
        assert!(render_workspace(40, 80, &ui).join("\n").contains("review"));
    }

    #[test]
    fn input_while_session_create_is_pending_cancels_automatic_closeup() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(SnapshotSessionPort(calls)),
            ModalSelectionMode::Action,
        );
        while !ui.workspace.new_session_selected() {
            assert_eq!(step_workspace(&mut ui, Key::Down), WorkspaceStep::Stay);
        }
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);
        for character in "review".chars() {
            assert_eq!(
                step_workspace(&mut ui, Key::Char(character)),
                WorkspaceStep::Stay
            );
        }
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);
        assert_eq!(ui.workspace.pending_session(), Some("review"));

        // This input is handled while the worker is still pending, so a later
        // successful completion updates the sidebar but leaves navigation here.
        assert_eq!(step_workspace(&mut ui, Key::Up), WorkspaceStep::Stay);
        while ui.workspace.pending_session().is_some() {
            drain_session_completions(&mut ui);
            std::thread::yield_now();
        }

        assert!(ui.workspace.selected_session().is_none());
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Switch);
    }

    #[test]
    fn metrics_port_refreshes_the_workspace_sidebar() {
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(RecordingSessionPort(Arc::new(Mutex::new(Vec::new())))),
            ModalSelectionMode::Action,
        )
        .with_metrics_port(Box::new(StaticMetrics));

        refresh_metrics(&mut ui);

        let rendered = render_workspace(40, 80, &ui).join("\n");
        assert!(rendered.contains('\u{f2db}'));
        assert!(rendered.contains('\u{f233}'));
        assert!(rendered.contains("45MB"));
        assert!(!rendered.contains('・'));
    }

    #[test]
    fn new_session_row_edits_inline_and_dispatches_the_create_command() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(SnapshotSessionPort(calls.clone())),
            ModalSelectionMode::Action,
        );
        while !ui.workspace.new_session_selected() {
            assert_eq!(step_workspace(&mut ui, Key::Down), WorkspaceStep::Stay);
        }

        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);
        assert!(ui.workspace.creating_session_inline());
        assert!(ui.modal.is_none());
        assert!(render_workspace(40, 80, &ui).join("\n").contains("+ new:"));

        for character in "review".chars() {
            assert_eq!(
                step_workspace(&mut ui, Key::Char(character)),
                WorkspaceStep::Stay
            );
        }
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);
        while ui.workspace.pending_session().is_some() {
            drain_session_completions(&mut ui);
            std::thread::yield_now();
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                "alpha".to_owned(),
                None,
                SessionCommand::Create {
                    name: "review".to_owned()
                }
            )]
        );
    }

    #[test]
    fn typing_on_the_new_session_row_starts_inline_input_with_that_character() {
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        );
        while !ui.workspace.new_session_selected() {
            let _ = step_workspace(&mut ui, Key::Down);
        }

        assert_eq!(step_workspace(&mut ui, Key::Char('f')), WorkspaceStep::Stay);
        assert!(ui.workspace.creating_session_inline());
        assert_eq!(ui.workspace.inline_create_value(), Some("f"));
    }

    #[test]
    fn empty_inline_new_session_name_stays_open_with_a_safe_error() {
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        );
        while !ui.workspace.new_session_selected() {
            let _ = step_workspace(&mut ui, Key::Down);
        }
        let _ = step_workspace(&mut ui, Key::Enter);

        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);
        assert!(ui.workspace.creating_session_inline());
        assert!(
            render_workspace(40, 80, &ui)
                .join("\n")
                .contains("session name is required")
        );
    }

    #[test]
    fn ctrl_a_moves_the_cursor_to_the_inline_new_session_input() {
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        );
        assert!(ui.workspace.root_selected());

        assert_eq!(
            step_workspace(&mut ui, Key::Char('\u{1}')),
            WorkspaceStep::Stay
        );
        assert!(ui.workspace.new_session_selected());
        assert!(ui.workspace.creating_session_inline());
    }

    #[test]
    fn session_remove_select_opens_a_checklist_and_removes_only_the_checked_session() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(SnapshotSessionPort(calls.clone())),
            ModalSelectionMode::Prompt,
        );
        ui.open_overview();
        for ch in "session remove -s".chars() {
            assert_eq!(step_workspace(&mut ui, Key::Char(ch)), WorkspaceStep::Stay);
        }
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);
        assert!(matches!(ui.modal, Some(WorkspaceModal::Remove(_))));
        assert!(
            render_workspace(40, 80, &ui)
                .join("\n")
                .contains("Remove sessions")
        );

        // The checklist owns input: a live action cannot open a pane behind it.
        assert_eq!(
            step_workspace(&mut ui, Key::Live(LiveTerminalAction::Agent)),
            WorkspaceStep::Stay
        );
        assert!(ui.workspace.pane().tabs().is_empty());
        assert_eq!(step_workspace(&mut ui, Key::Char(' ')), WorkspaceStep::Stay);
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);

        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                "alpha".to_owned(),
                Some("alpha-session".to_owned()),
                SessionCommand::Remove {
                    name: "alpha-session".to_owned(),
                    force: false,
                },
            )]
        );
        assert!(ui.modal.is_none());
    }

    #[test]
    fn session_remove_name_dispatches_the_named_target_without_using_the_cursor() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(SnapshotSessionPort(calls.clone())),
            ModalSelectionMode::Prompt,
        );
        ui.open_overview();
        for ch in "session remove alpha-session --force".chars() {
            assert_eq!(step_workspace(&mut ui, Key::Char(ch)), WorkspaceStep::Stay);
        }
        assert_eq!(step_workspace(&mut ui, Key::Enter), WorkspaceStep::Stay);

        assert_eq!(
            *calls.lock().unwrap(),
            vec![(
                "alpha".to_owned(),
                None,
                SessionCommand::Remove {
                    name: "alpha-session".to_owned(),
                    force: true,
                },
            )]
        );
        assert!(ui.workspace.sessions().is_empty());
    }

    #[test]
    fn session_remove_select_escape_restores_the_underlying_closeup() {
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        );
        ui.enter_closeup();
        ui.open_remove_selector(false);
        assert_eq!(step_workspace(&mut ui, Key::Escape), WorkspaceStep::Stay);
        assert!(ui.modal.is_none());
        assert_eq!(ui.workspace.mode(), WorkspaceMode::Closeup);
    }

    #[test]
    fn closeup_close_uses_the_shared_selector_and_accepts_each_force_flag_once() {
        let workspace = WorkspaceView::new(ws("alpha"), state("alpha"));
        let mut ui = WorkspaceUi::with_ports_and_selection_mode(
            workspace,
            Box::new(SnapshotOverlayData),
            Box::new(UnavailableSessionCommandPort),
            ModalSelectionMode::Action,
        );
        ui.enter_closeup();

        execute_closeup_command(&mut ui, "close -f");
        assert!(matches!(
            ui.modal,
            Some(WorkspaceModal::Remove(ref modal)) if modal.force()
        ));

        ui.modal = None;
        execute_closeup_command(&mut ui, "close --force");
        assert!(matches!(
            ui.modal,
            Some(WorkspaceModal::Remove(ref modal)) if modal.force()
        ));

        ui.modal = None;
        execute_closeup_command(&mut ui, "close alpha -f");
        assert!(ui.modal.is_none());
        execute_closeup_command(&mut ui, "close -f --force");
        assert!(ui.modal.is_none());
    }

    #[test]
    fn workspace_overlays_and_diff_tab_keep_the_home_surface_visible() {
        let keys = [
            Key::Down,
            Key::Char('v'),
            Key::Down,
            Key::Escape,
            Key::Char('d'),
            Key::Char('n'),
            Key::Down,
            Key::Escape,
            Key::Char('p'),
            Key::Escape,
            Key::Char('q'),
            Key::Enter,
        ];
        let mut term = FakeTerminal::with_keys(&keys);

        assert_eq!(
            run_workspace(&mut term, snapshot_with_pr("overlays")).unwrap(),
            Exit::Quit
        );
        let frame = |index: usize| term.frames[index].join("\n");

        assert!(frame(2).contains("Preview"));
        assert!(frame(2).contains("session: overlays-session"));
        assert!(frame(2).contains("overlays-session")); // Home background remains visible.
        assert!(strip_ansi(&frame(5)).contains("Diff"));
        assert!(frame(6).contains("Notes"));
        assert!(frame(6).contains("No notes are available"));
        assert!(frame(9).contains("Pull Request"));
        assert!(frame(9).contains("#42"));
    }

    #[test]
    fn workspace_accepts_an_injected_overlay_data_port() {
        let mut term =
            FakeTerminal::with_keys(&[Key::Char('v'), Key::Escape, Key::Char('q'), Key::Enter]);
        assert_eq!(
            run_workspace_with_overlay_data(
                &mut term,
                snapshot("injected"),
                Box::new(FixedOverlayData),
            )
            .unwrap(),
            Exit::Quit
        );
        assert!(term.frames[1].join("\n").contains("injected preview"));
    }

    #[test]
    fn direct_workspace_handles_navigation_shortcuts_and_exit_keys() {
        for (navigation, exit) in [
            (vec![Key::Escape, Key::Escape], Key::Quit),
            (Vec::new(), Key::Quit),
            (Vec::new(), Key::Char('q')),
        ] {
            let mut keys = vec![
                Key::Char('j'),
                Key::Char('k'),
                Key::Char('l'),
                Key::Char('h'),
                Key::Char('t'),
                Key::Right,
                Key::Left,
                Key::Char('k'),
                Key::Char('j'),
                Key::Char('z'),
                Key::Click { column: 0, row: 5 },
                Key::Other,
            ];
            keys.extend(navigation);
            keys.push(exit.clone());
            if exit == Key::Char('q') {
                keys.push(Key::Enter);
            }
            let mut term = FakeTerminal::with_keys(&keys);
            assert_eq!(
                run_workspace(&mut term, snapshot("direct")).unwrap(),
                Exit::Quit
            );
            assert_eq!(term.frames.len(), keys.len());
            assert!(term.frames[0].join("\n").contains("direct-session"));
        }
    }

    #[test]
    fn runtime_io_failures_are_propagated() {
        let mut size_failure = FakeTerminal {
            fail_size: true,
            ..FakeTerminal::default()
        };
        assert_eq!(
            run_workspace(&mut size_failure, snapshot("x"))
                .unwrap_err()
                .to_string(),
            "size failed"
        );

        let mut draw_failure = FakeTerminal {
            fail_draw: true,
            ..FakeTerminal::default()
        };
        assert_eq!(
            run(
                &mut draw_failure,
                Vec::new(),
                Vec::new(),
                now(),
                &mut FakeLoader::default(),
            )
            .unwrap_err()
            .to_string(),
            "draw failed"
        );

        let mut read_failure = FakeTerminal::default();
        assert_eq!(
            run_workspace(&mut read_failure, snapshot("x"))
                .unwrap_err()
                .to_string(),
            "no more keys"
        );
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

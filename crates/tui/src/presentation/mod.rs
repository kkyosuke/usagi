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

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use chrono::{DateTime, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::recent::Recent;
use usagi_core::domain::settings::ModalSelectionMode;
use usagi_core::domain::workspace::Workspace;

use crate::presentation::views::closeup_modal::{self, CloseupModal};
use crate::presentation::views::config::{self, Config};
use crate::presentation::views::new::{self, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::overview_modal::{self, OverviewModal};
use crate::presentation::views::pr_modal::{self, PrModal};
use crate::presentation::views::text_overlay::{self, OverlayDocument, TextOverlay};
use crate::presentation::views::welcome::{self, MenuAction, Welcome};
use crate::presentation::views::workspace::{self, Mode, Workspace as WorkspaceView};
use crate::usecase::application::pane::PaneKind;
use crate::usecase::application::{Key, ScreenRunner, Terminal};
use crate::usecase::closeup;
use crate::usecase::overview::{self, SessionCommand};
use crate::usecase::terminal_input::LiveTerminalAction;
use usagi_core::usecase::settings::SettingsPort;

pub use crate::usecase::application::{WorkspaceLoader, WorkspaceSnapshot};

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
    Pr(PrModal),
    Text(TextOverlay),
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
}

impl SessionCommandResult {
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            sessions: None,
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
    skeleton_frame: usize,
}

struct SessionCommandCompletion {
    port: Box<dyn SessionCommandPort>,
    result: Result<SessionCommandResult, String>,
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
            skeleton_frame: 0,
        }
    }

    /// 選択中の行を対象に Closeup へ入り、action menu を先頭から開く。
    ///
    /// tab が無い target では action modal がそのまま前面に出る。tab がある target
    /// では tab を前面にするため、`closeup_action_forced` は倒したまま入る。
    #[coverage(off)]
    fn enter_closeup(&mut self) {
        self.workspace.enter_closeup();
        self.closeup = CloseupModal::with_selection_mode(
            self.workspace.focused_label(),
            self.modal_selection_mode,
        );
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
        let document = self.overlay_data.diff(&self.workspace);
        self.modal = Some(WorkspaceModal::Text(TextOverlay::new("Diff", document)));
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
        Key::Quit => ConfigStep::Quit,
        _ => ConfigStep::Stay,
    }
}

/// welcome 画面のキー処理。最上位画面なので Esc も終了として扱う。
#[coverage(off)]
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
        Key::Escape | Key::Quit => WelcomeStep::Quit,
        Key::Enter => welcome_action(welcome.selected_action()),
        Key::Char(ch) => welcome
            .action_for(ch)
            .map_or(WelcomeStep::Stay, welcome_action),
        Key::Left | Key::Right | Key::Backspace | Key::Tab | Key::Live(_) | Key::Other => {
            WelcomeStep::Stay
        }
    }
}

/// New 画面のキー処理（純粋）。上下でフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
#[coverage(off)]
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
        Key::Quit => NewStep::Quit,
        Key::Enter | Key::Tab | Key::Live(_) | Key::Other => NewStep::Stay,
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
fn step_open(open: &mut Open, key: Key) -> OpenStep {
    if open.cleanup_confirming() {
        return match key {
            Key::Char('y') | Key::Enter => OpenStep::ConfirmCleanup,
            Key::Char('n') | Key::Escape => {
                open.cancel_cleanup();
                OpenStep::Stay
            }
            Key::Quit => OpenStep::Quit,
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
        Key::Quit => OpenStep::Quit,
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
        Key::Char(ch) => {
            open.push_filter(ch);
            OpenStep::Stay
        }
        Key::Live(_) | Key::Other => OpenStep::Stay,
    }
}

/// Overview modal の入力処理。文字入力中の `q` を含め、modal が全キーを先に受け取る。
#[coverage(off)]
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
        Key::Enter => {
            let input = modal.submission();
            modal.record_submission();
            match overview::interpret(&input) {
                Ok(overview::Command::Session { arguments }) => {
                    match overview::parse_session(&arguments) {
                        Ok(command @ SessionCommand::Create { .. }) => {
                            begin_session_create(ui, command);
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
        Key::Quit | Key::Live(_) | Key::Other => {}
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
    if let Some(sessions) = result.sessions {
        ui.workspace.replace_sessions(sessions);
    }
    if let Some(WorkspaceModal::Overview(modal)) = ui.modal.as_mut() {
        modal.set_result(result.message);
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
                if let Some(sessions) = result.sessions {
                    ui.workspace.replace_sessions(sessions);
                }
                ui.modal = None;
            }
            Err(error) => {
                if let Some(WorkspaceModal::Overview(modal)) = ui.modal.as_mut() {
                    modal.set_error(error);
                }
            }
        }
    }
}

/// Input-only Overview reducer retained for modal rendering scenarios. Runtime
/// execution uses [`step_overview_command`] so session commands reach its port.
#[cfg(test)]
#[coverage(off)]
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
        Key::Quit | Key::Live(_) | Key::Other => {}
    }
    false
}

/// PR modal の入力処理。Enter のブラウザ起動は外部 IO port が接続されるまで no-op とする。
#[coverage(off)]
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
        | Key::Char(_)
        | Key::Live(_)
        | Key::Other => {}
    }
    false
}

/// 長文 overlay の入力処理。背景の cursor / tab は動かさない。
#[coverage(off)]
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
        | Key::Char(_)
        | Key::Live(_)
        | Key::Other => {}
    }
    false
}

/// Switch のキー処理。session 選択と preview tab の移動を行い、Enter / `t` で
/// 選択行の Closeup action menu へ入る。基底の workspace は back stack の終端なので、
/// Esc はここから抜けず no-op とする。
#[coverage(off)]
fn step_switch(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    match key {
        Key::Up | Key::Char('k') => ui.workspace.select_prev(),
        Key::Down | Key::Char('j') => ui.workspace.select_next(),
        Key::Left | Key::Char('h') => ui.workspace.tab_prev(),
        Key::Right | Key::Char('l') => ui.workspace.tab_next(),
        Key::Enter | Key::Char('t') => ui.enter_closeup(),
        Key::Char(':') => ui.open_overview(),
        Key::Char('p') => ui.open_prs(),
        Key::Char('v') => ui.open_preview(),
        Key::Char('d') => ui.open_diff(),
        Key::Char('n') => ui.open_text(),
        Key::Quit | Key::Char('q') => return WorkspaceStep::Quit,
        // Live-terminal prefix actions are Closeup-scoped; Switch ignores them.
        Key::Escape | Key::Backspace | Key::Tab | Key::Char(_) | Key::Live(_) | Key::Other => {}
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
            Key::Char(ch) => ui.closeup.insert_char(ch),
            Key::Escape => close_closeup_modal(ui),
            Key::Quit => return WorkspaceStep::Quit,
            Key::Enter => {
                let input = ui.closeup.submission();
                execute_closeup_command(ui, &input);
            }
            Key::Up | Key::Down | Key::Tab | Key::Live(_) | Key::Other => {}
        }
        return WorkspaceStep::Stay;
    }
    step_closeup_menu(ui, key)
}

/// action modal が前面のときの menu 操作。Enter は選択 action で pane を開き、開いた後は
/// forced modal を倒して新しい tab を前面へ出す。
#[coverage(off)]
fn step_closeup_menu(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    match key {
        Key::Up | Key::Char('k') => ui.closeup.select_prev(),
        Key::Down | Key::Char('j') => ui.closeup.select_next(),
        Key::Left | Key::Char('h') => ui.workspace.tab_prev(),
        Key::Right | Key::Char('l') => ui.workspace.tab_next(),
        Key::Char(':') => ui.open_overview(),
        Key::Char('p') => ui.open_prs(),
        Key::Char('v') => ui.open_preview(),
        Key::Char('d') => ui.open_diff(),
        Key::Char('n') => ui.open_text(),
        Key::Escape => close_closeup_modal(ui),
        Key::Quit | Key::Char('q') => return WorkspaceStep::Quit,
        Key::Enter => {
            let input = ui.closeup.submission();
            execute_closeup_command(ui, &input);
        }
        Key::Char('x') => ui.workspace.close_pane(),
        Key::Backspace | Key::Tab | Key::Char(_) | Key::Live(_) | Key::Other => {}
    }
    WorkspaceStep::Stay
}

/// tab が前面のときの操作。左右で tab を巡回し、`x` で閉じる。overlay / quit は共通。
/// action menu は前面に無いので上下・Enter は無視する。
#[coverage(off)]
fn step_closeup_tabs(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    match key {
        Key::Left | Key::Char('h') => ui.workspace.tab_prev(),
        Key::Right | Key::Char('l') => ui.workspace.tab_next(),
        Key::Char('x') => ui.workspace.close_pane(),
        Key::Char(':') => ui.open_overview(),
        Key::Char('p') => ui.open_prs(),
        Key::Char('v') => ui.open_preview(),
        Key::Char('d') => ui.open_diff(),
        Key::Char('n') => ui.open_text(),
        Key::Quit | Key::Char('q') => return WorkspaceStep::Quit,
        Key::Escape
        | Key::Up
        | Key::Down
        | Key::Enter
        | Key::Backspace
        | Key::Tab
        | Key::Char(_)
        | Key::Live(_)
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
            ui.closeup_action_forced = false;
            ui.workspace.enter_switch();
        }
        LiveTerminalAction::OpenCloseupModal => {
            ui.closeup = CloseupModal::with_selection_mode(
                ui.workspace.focused_label(),
                ui.modal_selection_mode,
            );
            ui.closeup_action_forced = true;
        }
        LiveTerminalAction::NextTab => ui.workspace.tab_next(),
        LiveTerminalAction::PreviousTab => ui.workspace.tab_prev(),
        LiveTerminalAction::Agent => open_pane_from_menu(ui, PaneKind::Agent),
        LiveTerminalAction::CloseTab => ui.workspace.close_pane(),
        LiveTerminalAction::QuitConfirmation => return WorkspaceStep::Quit,
        LiveTerminalAction::PreviousSession => ui.workspace.select_prev(),
    }
    WorkspaceStep::Stay
}

/// Open a pane and hide the (possibly forced) action modal so the new tab is front.
#[coverage(off)]
fn open_pane_from_menu(ui: &mut WorkspaceUi, kind: PaneKind) {
    ui.workspace.open_pane(kind);
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
        Ok(closeup::Command::Agent { .. }) => open_pane_from_menu(ui, PaneKind::Agent),
        Ok(closeup::Command::Terminal { .. }) => open_pane_from_menu(ui, PaneKind::Terminal),
        Ok(closeup::Command::Close { .. } | closeup::Command::Diff { .. }) | Err(_) => {}
    }
}

/// Workspace 画面のキー処理。Ctrl-C は常に終了し、それ以外は最前面 modal、現在 mode の
/// 順に dispatch する。これにより背面の session / tab が modal 操作で動かない。
#[coverage(off)]
fn step_workspace(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    if key == Key::Quit {
        return WorkspaceStep::Quit;
    }

    if let Some(modal) = &mut ui.modal {
        let close = match modal {
            WorkspaceModal::Overview(_) => step_overview_command(ui, key),
            WorkspaceModal::Pr(modal) => step_pr(modal, key),
            WorkspaceModal::Text(modal) => step_text_overlay(modal, key),
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
        Some(WorkspaceModal::Text(modal)) => text_overlay::render_over(height, width, &base, modal),
        None if ui.closeup_modal_visible() => {
            closeup_modal::render_over(height, width, &base, &ui.closeup)
        }
        None => base,
    }
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
    loop {
        drain_session_completions(&mut ui);
        let (height, width) = term.size()?;
        term.draw(&render_workspace(height, width, &ui))?;
        let key = term.read_key()?;
        ui.skeleton_frame = ui.skeleton_frame.wrapping_add(1);
        match step_workspace(&mut ui, key) {
            WorkspaceStep::Stay => {}
            WorkspaceStep::Quit => return Ok(WorkspaceStep::Quit),
        }
    }
}

/// Workspace を起点にした公開 runtime。direct `usagi open <path>` は合成側で [`WorkspaceLoader`]
/// を一度呼び、その snapshot をこの関数へ渡す。基底の Switch で Esc を押しても workspace
/// からは抜けず、終了には `q` / Ctrl-C を使う。
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
    drive_workspace_with_ports(
        term,
        snapshot,
        Box::new(SnapshotOverlayData),
        session_commands,
    )
    .map(|_| Exit::Quit)
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
/// Closeup や前面 modal を閉じるためだけに使う。`q` / Ctrl-C はどの画面でも runtime 全体を
/// 終了する。
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
    let mut welcome = Welcome::new(recent);
    let mut open = open_from_registry(workspaces, welcome.recent());
    let mut new_form = New::default();
    let mut config_form = Config::load(settings);
    let mut screen = match start {
        Start::Welcome => Screen::Welcome,
        Start::Config => Screen::Config,
    };
    loop {
        let (height, width) = term.size()?;
        let frame = match screen {
            Screen::Welcome => welcome::render(height, width, &welcome, now),
            Screen::Open => open::render(height, width, &open, now),
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
                    if drive_workspace_with_ports_and_selection_mode(
                        term,
                        snapshot,
                        Box::new(SnapshotOverlayData),
                        session_commands.create(),
                        config_form.global_modal_selection_mode(),
                    )? == WorkspaceStep::Quit
                    {
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
                        if drive_workspace_with_ports_and_selection_mode(
                            term,
                            snapshot,
                            Box::new(SnapshotOverlayData),
                            session_commands.create(),
                            config_form.global_modal_selection_mode(),
                        )? == WorkspaceStep::Quit
                        {
                            return Ok(Exit::Quit);
                        }
                    }
                }
                OpenStep::ConfirmCleanup => {
                    let removed = loader.cleanup_missing(&open.workspaces())?;
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
mod tests {
    use super::{
        BannerScreenRunner, Config, ConfigStep, DefaultSettingsPort, Exit, NewStep,
        OverlayDataPort, OverlayDocument, OverviewModal, PrModal, SessionCommandPort,
        SessionCommandPortFactory, SessionCommandResult, SnapshotOverlayData, Start,
        UnavailableSessionCommandPort, WelcomeStep, WorkspaceLoader, WorkspaceModal,
        WorkspaceSnapshot, WorkspaceStep, WorkspaceUi, run as run_from_start, run_with_settings,
        run_workspace, run_workspace_with_overlay_data, run_workspace_with_session_port,
        step_config, step_new, step_overview, step_pr, step_workspace, welcome_action,
        write_banner,
    };
    use crate::presentation::views::new::{Field, Mode, New};
    use crate::presentation::views::welcome::MenuAction;
    use crate::presentation::views::workspace::{
        Mode as WorkspaceMode, Workspace as WorkspaceView,
    };
    use crate::usecase::application::run as dispatch;
    use crate::usecase::application::{EntryScreen, Key, Terminal};
    use crate::usecase::overview::SessionCommand;
    use chrono::{DateTime, Duration, Utc};
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::pullrequest::PrLink;
    use usagi_core::domain::recent::{Recent, UniteOverview};
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
    use usagi_core::domain::settings::ModalSelectionMode;
    use usagi_core::domain::workspace::{Workspace, WorkspaceOverview};
    use usagi_core::domain::workspace_state::WorkspaceState;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
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
            let created = match &command {
                SessionCommand::Create { name } => Some(name.clone()),
                _ => None,
            };
            self.0.lock().unwrap().push((
                workspace.name.clone(),
                selected.map(|session| session.name.clone()),
                command,
            ));
            let sessions = created.map(|name| {
                vec![SessionRecord {
                    name: name.clone(),
                    display_name: None,
                    origin: SessionOrigin::Human,
                    started_from: None,
                    root: workspace.path.join(".usagi/sessions").join(&name),
                    created_at: now(),
                    last_active: None,
                    notes: Scratchpad::default(),
                    prs: Vec::new(),
                }]
            });
            Ok(SessionCommandResult {
                message: "daemon accepted".to_owned(),
                sessions,
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
        fail_size: bool,
        fail_draw: bool,
    }

    impl FakeTerminal {
        fn with_keys(keys: &[Key]) -> Self {
            Self {
                keys: keys.iter().copied().collect(),
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

        fn read_key(&mut self) -> io::Result<Key> {
            self.keys
                .pop_front()
                .ok_or_else(|| io::Error::other("no more keys"))
        }
    }

    #[derive(Default)]
    struct FakeLoader {
        opened: Vec<PathBuf>,
        cleanup_removed: Vec<PathBuf>,
        cleanup_calls: usize,
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
    }

    #[test]
    fn run_quits_from_welcome_and_handles_menu_navigation() {
        for keys in [
            vec![Key::Char('q')],
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
    fn run_ignores_unknown_welcome_keys() {
        let keys = [
            Key::Char('z'),
            Key::Left,
            Key::Right,
            Key::Backspace,
            Key::Other,
            Key::Char('q'),
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
        assert_eq!(term.frames.len(), keys.len());
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
            FakeTerminal::with_keys(&[Key::Char('c'), Key::Escape, Key::Char('q')]);
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
    fn new_form_opens_edits_and_returns_to_welcome() {
        let keys = [
            Key::Char('e'),
            Key::Down,
            Key::Char('a'),
            Key::Backspace,
            Key::Escape,
            Key::Char('q'),
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
        let mut term = FakeTerminal::with_keys(&[Key::Char('o'), Key::Enter, Key::Char('q')]);
        let mut loader = FakeLoader::default();
        assert_eq!(
            run(&mut term, vec![ws("alpha")], Vec::new(), now(), &mut loader,).unwrap(),
            Exit::Quit
        );
        assert_eq!(loader.opened, vec![PathBuf::from("/tmp/alpha")]);
        assert_eq!(term.frames.len(), 3);
        assert!(term.frames[0].join("\n").contains("Menu"));
        assert!(term.frames[1].join("\n").contains("Open Workspace"));
        assert!(term.frames[2].join("\n").contains("alpha-session"));
    }

    #[test]
    fn open_filter_cleanup_confirmation_and_unite_selection_use_the_injected_loader() {
        let alpha = ws("alpha");
        let beta = ws("beta");

        let mut filter =
            FakeTerminal::with_keys(&[Key::Char('o'), Key::Char('b'), Key::Enter, Key::Char('q')]);
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
            Key::Quit,
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
        let keys = [Key::Char('o'), Key::Up, Key::Escape, Key::Char('q')];
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
        let keys = [Key::Char('o'), Key::Enter, Key::Escape, Key::Char('q')];
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
        let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Escape, Key::Char('q')]);
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
        let keys = [Key::Char('2'), Key::Escape, Key::Char('q')];
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
        let keys = [Key::Char('2'), Key::Char('1'), Key::Char('q')];
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
        let mut term = FakeTerminal::with_keys(&[Key::Char('3'), Key::Char('q')]);
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
        let mut term = FakeTerminal::with_keys(&[Key::Char('1'), Key::Char('q')]);
        run(
            &mut term,
            Vec::new(),
            vec![recent("recent")],
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert_eq!(term.frames.len(), 2);
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
            Key::Char('q'),
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
        assert!(frame(0).contains("Switch"));
        assert!(frame(0).contains("No tabs stirring yet. Enter starts one."));
        assert!(frame(2).contains("No tabs stirring yet. Enter starts one."));

        // Closeup modal は workspace と tab strip の上に重なり、左右移動後の tab を保つ。
        assert!(frame(3).contains("terminal"));
        assert!(frame(3).contains("direct-session"));

        // Overview が Closeup の上に重なり、q は終了せず入力として処理される。
        assert!(frame(5).contains("workspace commands"));
        assert!(frame(6).contains("no matching command"));
        assert!(frame(6).contains("Overview"));
        assert!(frame(9).contains("terminal"));

        // PR modal も実データを表示し、閉じると同じ Closeup に戻る。
        assert!(frame(10).contains("Pull Request"));
        assert!(frame(10).contains("#42"));
        assert!(frame(12).contains("terminal"));

        // Closeup 上の Esc は mode を変えない。終了は明示的な Quit のみ。
        assert!(frame(13).contains("\u{f00e} Closeup"));
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
                .any(|frame| frame.contains("Agent (starting)"))
        );
        assert!(
            terminal_frames
                .iter()
                .any(|frame| frame.contains("Terminal (resolving)"))
        );
        assert!(agent_frames.iter().any(|frame| frame.contains('▔')));
        assert!(
            agent_frames
                .iter()
                .any(|frame| frame.contains("No tabs stirring yet. Enter starts one."))
        );
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
        assert!(frame(3).contains("Agent (starting)"));
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
        assert_eq!(ui.workspace.pane().selected(), &before);

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
    fn closeup_prefix_quit_and_passthrough_keys_are_preserved() {
        // Quit still terminates, and a live prefix `q` maps to the quit action.
        use crate::usecase::terminal_input::LiveTerminalAction;

        let workspace = WorkspaceView::new(ws("prefix-quit"), state("prefix-quit"));
        let mut ui = WorkspaceUi::with_overlay_data(workspace, Box::new(SnapshotOverlayData));
        ui.enter_closeup();
        assert_eq!(
            step_workspace(&mut ui, Key::Live(LiveTerminalAction::QuitConfirmation)),
            WorkspaceStep::Quit
        );
    }

    #[test]
    fn overview_session_command_uses_the_injected_daemon_port() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let port = RecordingSessionPort(calls.clone());
        let mut keys = vec![Key::Char(':')];
        keys.extend("session list".chars().map(Key::Char));
        keys.extend([Key::Enter, Key::Quit]);
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
                .last()
                .unwrap()
                .join("\n")
                .contains("daemon accepted")
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
        let keys = [Key::Char('o'), Key::Enter, Key::Char('q')];
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

    /// Welcome の Recent 経由で開いた workspace も同じ factory から port を取り出す。
    #[test]
    fn recent_workspace_pulls_the_session_command_port_from_the_factory() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let created = Arc::new(Mutex::new(0usize));
        let mut factory = SnapshotSessionPortFactory {
            calls: calls.clone(),
            created: created.clone(),
        };
        let keys = [Key::Char('1'), Key::Char('q')];
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
    /// sidebar の session 行へ反映されることを固定する。
    #[test]
    fn session_create_reaches_the_port_and_snapshot_reflects_in_the_sidebar() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let port = SnapshotSessionPort(calls.clone());
        let mut keys = vec![Key::Char(':')];
        keys.extend("session create review".chars().map(Key::Char));
        keys.extend([Key::Enter, Key::Escape, Key::Char('q')]);
        let mut term = FakeTerminal::with_keys(&keys);

        assert_eq!(
            run_workspace_with_session_port(&mut term, snapshot("alpha"), Box::new(port)).unwrap(),
            Exit::Quit
        );

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
        assert!(term.frames.last().unwrap().join("\n").contains("review"));
    }

    #[test]
    fn workspace_text_overlays_keep_home_visible_and_capture_scroll_keys() {
        let keys = [
            Key::Down,
            Key::Char('v'),
            Key::Down,
            Key::Escape,
            Key::Char('d'),
            Key::Down,
            Key::Escape,
            Key::Char('n'),
            Key::Down,
            Key::Escape,
            Key::Char('p'),
            Key::Escape,
            Key::Quit,
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
        assert!(frame(5).contains("Diff"));
        assert!(frame(5).contains("unavailable until a backend"));
        assert!(frame(8).contains("Notes"));
        assert!(frame(8).contains("No notes are available"));
        assert!(frame(11).contains("Pull Request"));
        assert!(frame(11).contains("#42"));
    }

    #[test]
    fn workspace_accepts_an_injected_overlay_data_port() {
        let mut term = FakeTerminal::with_keys(&[Key::Char('v'), Key::Escape, Key::Quit]);
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
                Key::Other,
            ];
            keys.extend(navigation);
            keys.push(exit);
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

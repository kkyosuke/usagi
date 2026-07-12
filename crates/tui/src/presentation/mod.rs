#![coverage(off)]

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

use chrono::{DateTime, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::recent::Recent;
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
use crate::usecase::application::{Key, ScreenRunner, Terminal};
use usagi_core::usecase::settings::SettingsPort;

pub use crate::usecase::application::{WorkspaceLoader, WorkspaceSnapshot};

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
    Back,
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

/// 永続化済み snapshot を読む既定の overlay data port。
struct SnapshotOverlayData;

impl OverlayDataPort for SnapshotOverlayData {
    fn preview(&self, workspace: &WorkspaceView) -> OverlayDocument {
        OverlayDocument::Ready(workspace.focused_preview_lines())
    }

    fn diff(&self, _workspace: &WorkspaceView) -> OverlayDocument {
        OverlayDocument::Unavailable(
            "Diff data is unavailable until a backend supplies it.".to_string(),
        )
    }

    fn text(&self, workspace: &WorkspaceView) -> OverlayDocument {
        let lines = workspace.focused_note_lines();
        if lines.is_empty() {
            OverlayDocument::Unavailable("No notes are available for this target.".to_string())
        } else {
            OverlayDocument::Ready(lines)
        }
    }

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
    modal: Option<WorkspaceModal>,
    overlay_data: Box<dyn OverlayDataPort>,
}

impl WorkspaceUi {
    fn with_overlay_data(workspace: WorkspaceView, overlay_data: Box<dyn OverlayDataPort>) -> Self {
        let closeup = CloseupModal::new(workspace.focused_label());
        Self {
            workspace,
            closeup,
            modal: None,
            overlay_data,
        }
    }

    /// 選択中の行を対象に Closeup へ入り、action menu を先頭から開く。
    fn enter_closeup(&mut self) {
        self.workspace.enter_closeup();
        self.closeup = CloseupModal::new(self.workspace.focused_label());
        self.modal = None;
    }

    /// 現在 mode を保ったまま Workspace scope の command palette を重ねる。
    fn open_overview(&mut self) {
        self.modal = Some(WorkspaceModal::Overview(OverviewModal::new()));
    }

    /// 選択中セッションの PR 一覧を現在 mode の上へ重ねる。root は空一覧になる。
    fn open_prs(&mut self) {
        self.modal = Some(match self.overlay_data.pull_requests(&self.workspace) {
            Ok(prs) => WorkspaceModal::Pr(PrModal::new(prs)),
            Err(message) => WorkspaceModal::Text(TextOverlay::new(
                "Pull Request",
                OverlayDocument::Unavailable(message),
            )),
        });
    }

    fn open_preview(&mut self) {
        let document = self.overlay_data.preview(&self.workspace);
        self.modal = Some(WorkspaceModal::Text(TextOverlay::new("Preview", document)));
    }

    fn open_diff(&mut self) {
        let document = self.overlay_data.diff(&self.workspace);
        self.modal = Some(WorkspaceModal::Text(TextOverlay::new("Diff", document)));
    }

    fn open_text(&mut self) {
        let document = self.overlay_data.text(&self.workspace);
        self.modal = Some(WorkspaceModal::Text(TextOverlay::new("Notes", document)));
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

/// Config 画面のキー処理（純粋）。Esc で welcome へ戻り、`Ctrl-C` で終了する。設定項目は
/// まだ無いので、その他のキーは留まる。
fn step_config(config: &mut Config, key: Key, settings: &mut dyn SettingsPort) -> ConfigStep {
    match key {
        Key::Tab => {
            config.toggle_scope();
            ConfigStep::Stay
        }
        Key::Left => {
            config.cycle_theme(false);
            ConfigStep::Stay
        }
        Key::Right => {
            config.cycle_theme(true);
            ConfigStep::Stay
        }
        Key::Char('s' | 'S') => {
            config.save(settings);
            ConfigStep::Stay
        }
        Key::Escape => ConfigStep::Back,
        Key::Quit => ConfigStep::Quit,
        _ => ConfigStep::Stay,
    }
}

/// welcome 画面のキー処理。最上位画面なので Esc も終了として扱う。
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
        Key::Left | Key::Right | Key::Backspace | Key::Tab | Key::Other => WelcomeStep::Stay,
    }
}

/// New 画面のキー処理（純粋）。上下でフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
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
        Key::Enter | Key::Tab | Key::Other => NewStep::Stay,
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
    if open.filtering() {
        return match key {
            Key::Char(ch) => {
                open.push_filter(ch);
                OpenStep::Stay
            }
            Key::Backspace => {
                open.pop_filter();
                OpenStep::Stay
            }
            Key::Enter | Key::Escape => {
                open.end_filter();
                OpenStep::Stay
            }
            Key::Quit => OpenStep::Quit,
            Key::Up | Key::Down | Key::Left | Key::Right | Key::Tab | Key::Other => OpenStep::Stay,
        };
    }
    match key {
        Key::Up | Key::Char('k') => {
            open.select_prev();
            OpenStep::Stay
        }
        Key::Down | Key::Char('j') => {
            open.select_next();
            OpenStep::Stay
        }
        Key::Escape => OpenStep::Back,
        Key::Quit | Key::Char('q') => OpenStep::Quit,
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
        Key::Char('/') => {
            open.begin_filter();
            OpenStep::Stay
        }
        Key::Char('u') => {
            open.toggle_unite();
            OpenStep::Stay
        }
        Key::Char(' ') if open.is_unite() => {
            open.toggle_unite_member();
            OpenStep::Stay
        }
        Key::Char('c') => {
            open.request_cleanup();
            OpenStep::Stay
        }
        Key::Char(_) | Key::Left | Key::Right | Key::Backspace | Key::Tab | Key::Other => {
            OpenStep::Stay
        }
    }
}

/// Overview modal の入力処理。文字入力中の `q` を含め、modal が全キーを先に受け取る。
/// Enter の command 実行は command handler が接続されるまで no-op とする。
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
        Key::Quit | Key::Other => {}
    }
    false
}

/// PR modal の入力処理。Enter のブラウザ起動は外部 IO port が接続されるまで no-op とする。
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
        | Key::Other => {}
    }
    false
}

/// 長文 overlay の入力処理。背景の cursor / tab は動かさない。
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
        | Key::Other => {}
    }
    false
}

/// Switch のキー処理。session 選択と preview tab の移動を行い、Enter / `t` で
/// 選択行の Closeup action menu へ入る。
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
        Key::Escape => return WorkspaceStep::Back,
        Key::Quit | Key::Char('q') => return WorkspaceStep::Quit,
        Key::Backspace | Key::Tab | Key::Char(_) | Key::Other => {}
    }
    WorkspaceStep::Stay
}

/// Closeup のキー処理。action menu の上下選択と背面 tab の左右移動を行う。Esc は
/// Workspace 自体を閉じず Switch へ一段戻す。
fn step_closeup(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
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
        Key::Escape => ui.workspace.enter_switch(),
        Key::Quit | Key::Char('q') => return WorkspaceStep::Quit,
        // action 実行は Closeup command handler が接続されるまで no-op。
        Key::Enter | Key::Backspace | Key::Tab | Key::Char(_) | Key::Other => {}
    }
    WorkspaceStep::Stay
}

/// Workspace 画面のキー処理。Ctrl-C は常に終了し、それ以外は最前面 modal、現在 mode の
/// 順に dispatch する。これにより背面の session / tab が modal 操作で動かない。
fn step_workspace(ui: &mut WorkspaceUi, key: Key) -> WorkspaceStep {
    if key == Key::Quit {
        return WorkspaceStep::Quit;
    }

    if let Some(modal) = &mut ui.modal {
        let close = match modal {
            WorkspaceModal::Overview(modal) => step_overview(modal, key),
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
fn render_workspace(height: usize, width: usize, ui: &WorkspaceUi) -> Vec<String> {
    let base = workspace::render(height, width, &ui.workspace);
    match &ui.modal {
        Some(WorkspaceModal::Overview(modal)) => {
            overview_modal::render_over(height, width, &base, modal)
        }
        Some(WorkspaceModal::Pr(modal)) => pr_modal::render_over(height, width, &base, modal),
        Some(WorkspaceModal::Text(modal)) => text_overlay::render_over(height, width, &base, modal),
        None if ui.workspace.mode() == Mode::Closeup => {
            closeup_modal::render_over(height, width, &base, &ui.closeup)
        }
        None => base,
    }
}

/// Recent が指す単体 workspace path。Unite の runtime は今回の対象外なので開かない。
fn recent_path(recent: &Recent) -> Option<&Path> {
    match recent {
        Recent::Workspace(overview) => Some(&overview.workspace.path),
        Recent::Unite(_) => None,
    }
}

/// 1 つの Workspace snapshot を、終了または Esc まで同じ Terminal 上で駆動する。
fn drive_workspace(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
) -> io::Result<WorkspaceStep> {
    drive_workspace_with_overlay_data(term, snapshot, Box::new(SnapshotOverlayData))
}

/// `overlay_data` を注入して 1 つの Workspace snapshot を駆動する。
///
/// diff / PR の backend fetch は実装しない。この seam に安全な projection を実装して
/// 注入することで、表示層を外部 IO や生エラーから分離する。
///
/// # Errors
///
/// 端末への描画またはキー読み取りに失敗した場合、そのエラーを返す。
pub fn run_workspace_with_overlay_data(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    overlay_data: Box<dyn OverlayDataPort>,
) -> io::Result<Exit> {
    drive_workspace_with_overlay_data(term, snapshot, overlay_data).map(|_| Exit::Quit)
}

fn drive_workspace_with_overlay_data(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    overlay_data: Box<dyn OverlayDataPort>,
) -> io::Result<WorkspaceStep> {
    let workspace = WorkspaceView::new(snapshot.workspace, snapshot.state);
    let mut ui = WorkspaceUi::with_overlay_data(workspace, overlay_data);
    loop {
        let (height, width) = term.size()?;
        term.draw(&render_workspace(height, width, &ui))?;
        match step_workspace(&mut ui, term.read_key()?) {
            WorkspaceStep::Stay => {}
            exit => return Ok(exit),
        }
    }
}

/// Workspace を起点にした公開 runtime。direct `usagi open <path>` は合成側で [`WorkspaceLoader`]
/// を一度呼び、その snapshot をこの関数へ渡す。起点より前の画面が無いため Esc も終了となる。
///
/// # Errors
///
/// 端末への描画またはキー読み取りに失敗した場合、そのエラーを返す。
pub fn run_workspace(term: &mut dyn Terminal, snapshot: WorkspaceSnapshot) -> io::Result<Exit> {
    drive_workspace(term, snapshot).map(|_| Exit::Quit)
}

/// `start` で選んだ画面を起点にした対話 runtime。
///
/// Welcome→Open→Workspace と Welcome→Recent→Workspace は選択 path を同じ [`WorkspaceLoader`]
/// で開き、同じ Workspace runtime を駆動する。Workspace で Esc を押すと呼び出し元へ戻るため、
/// Open 経由なら Open、Recent 経由なら Welcome が再描画される。`q` / Ctrl-C はどの画面でも
/// runtime 全体を終了する。
///
/// `workspaces` / `recent` / `now` は永続化・実時計を持つ呼び出し側から渡す。
///
/// # Errors
///
/// workspace の読み込み、端末への描画、キー読み取りのいずれかに失敗した場合、そのエラーを返す。
pub fn run_with_settings(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
) -> io::Result<Exit> {
    let mut welcome = Welcome::new(recent);
    let mut open = Open::new(workspaces);
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
                    if drive_workspace(term, snapshot)? == WorkspaceStep::Quit {
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
                        if drive_workspace(term, snapshot)? == WorkspaceStep::Quit {
                            return Ok(Exit::Quit);
                        }
                    }
                }
                OpenStep::ConfirmCleanup => {
                    let removed = loader.cleanup_missing(open.workspaces())?;
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
pub fn run(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
) -> io::Result<Exit> {
    let mut settings = DefaultSettingsPort;
    run_with_settings(term, workspaces, recent, now, start, loader, &mut settings)
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
    use super::{
        BannerScreenRunner, Config, ConfigStep, DefaultSettingsPort, Exit, NewStep,
        OverlayDataPort, OverlayDocument, OverviewModal, PrModal, SnapshotOverlayData, Start,
        WelcomeStep, WorkspaceLoader, WorkspaceModal, WorkspaceSnapshot, WorkspaceStep,
        WorkspaceUi, run as run_from_start, run_workspace, run_workspace_with_overlay_data,
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
    use chrono::{DateTime, Duration, Utc};
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::pullrequest::PrLink;
    use usagi_core::domain::recent::{Recent, UniteOverview};
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
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

        fn diff(&self, _workspace: &WorkspaceView) -> OverlayDocument {
            OverlayDocument::Unavailable("injected diff fallback".to_string())
        }

        fn text(&self, _workspace: &WorkspaceView) -> OverlayDocument {
            OverlayDocument::Ready(vec!["injected text".to_string()])
        }

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

        let mut filter = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Char('/'),
            Key::Char('b'),
            Key::Enter,
            Key::Char('q'),
        ]);
        run(
            &mut filter,
            vec![alpha.clone(), beta.clone()],
            Vec::new(),
            now(),
            &mut FakeLoader::default(),
        )
        .unwrap();
        assert!(filter.frames[3].join("\n").contains("↳ /tmp/beta"));

        let mut cancel = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Char('c'),
            Key::Char('n'),
            Key::Char('q'),
        ]);
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

        let mut confirm = FakeTerminal::with_keys(&[
            Key::Char('o'),
            Key::Char('c'),
            Key::Char('y'),
            Key::Char('q'),
        ]);
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
            Key::Char('u'),
            Key::Char(' '),
            Key::Down,
            Key::Char(' '),
            Key::Enter,
            Key::Escape,
            Key::Escape,
            Key::Char('q'),
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
    fn open_navigation_and_workspace_escape_return_to_open() {
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
            Key::Escape,
            Key::Left,
            Key::Right,
            Key::Char('z'),
            Key::Other,
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
                .contains("Open Workspace")
        );
        assert!(
            term.frames
                .iter()
                .any(|frame| frame.join("\n").contains("Terminal — workspace 'root'"))
        );
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
    fn open_touch_refreshes_open_and_welcome_recency_models() {
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
            Key::Escape,
            Key::Char('q'),
        ];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            opened_at: Some(now()),
            ..FakeLoader::default()
        };

        run(&mut term, vec![alpha, beta], recent, now(), &mut loader).unwrap();

        let refreshed_open = term.frames[3].join("\n");
        assert!(refreshed_open.contains("↳ /tmp/alpha"));
        assert!(refreshed_open.contains("just now"));
        assert!(refreshed_open.find("alpha").unwrap() < refreshed_open.find("beta").unwrap());

        let refreshed_welcome = term.frames[4].join("\n");
        assert!(refreshed_welcome.contains("just now"));
        assert!(refreshed_welcome.find("alpha").unwrap() < refreshed_welcome.find("beta").unwrap());
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
    fn recent_loads_workspace_and_escape_returns_to_welcome() {
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
        assert!(term.frames[2].join("\n").contains("Recent"));
    }

    #[test]
    fn recent_touch_refreshes_welcome_and_open_recency_models() {
        let alpha = ws_minutes_ago("alpha", 20);
        let beta = ws_minutes_ago("beta", 10);
        let recent = vec![
            Recent::Workspace(WorkspaceOverview::new(beta.clone(), 2, 3, 4)),
            Recent::Workspace(WorkspaceOverview::new(alpha.clone(), 5, 6, 7)),
        ];
        let keys = [Key::Char('2'), Key::Escape, Key::Char('o'), Key::Char('q')];
        let mut term = FakeTerminal::with_keys(&keys);
        let mut loader = FakeLoader {
            opened_at: Some(now()),
            ..FakeLoader::default()
        };

        run(&mut term, vec![beta, alpha], recent, now(), &mut loader).unwrap();

        let refreshed_welcome = term.frames[2].join("\n");
        assert!(refreshed_welcome.contains("just now"));
        assert!(refreshed_welcome.find("alpha").unwrap() < refreshed_welcome.find("beta").unwrap());

        let refreshed_open = term.frames[3].join("\n");
        assert!(refreshed_open.contains("↳ /tmp/beta"));
        assert!(refreshed_open.contains("just now"));
        assert!(refreshed_open.find("alpha").unwrap() < refreshed_open.find("beta").unwrap());
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
            Key::Escape,
        ];
        let mut term = FakeTerminal::with_keys(&keys);

        assert_eq!(
            run_workspace(&mut term, snapshot_with_pr("direct")).unwrap(),
            Exit::Quit
        );
        assert_eq!(term.frames.len(), keys.len());

        let frame = |index: usize| term.frames[index].join("\n");
        assert!(frame(0).contains("Switch"));
        assert!(frame(0).contains("Preview"));
        assert!(frame(1).contains("Terminal — session 'direct-session'"));

        // Closeup modal は workspace と tab strip の上に重なり、左右移動後の tab を保つ。
        assert!(frame(2).contains("terminal"));
        assert!(frame(2).contains("direct-session"));
        assert!(frame(2).contains("Terminal"));

        // Overview が Closeup の上に重なり、q は終了せず入力として処理される。
        assert!(frame(4).contains("workspace commands"));
        assert!(frame(5).contains("no matching command"));
        assert!(frame(5).contains("Command"));
        assert!(frame(8).contains("terminal"));

        // PR modal も実データを表示し、閉じると同じ Closeup に戻る。
        assert!(frame(9).contains("Pull Request"));
        assert!(frame(9).contains("#42"));
        assert!(frame(9).contains("Terminal"));
        assert!(frame(11).contains("terminal"));

        // 次の Esc は Closeup -> Switch。最後の Esc が direct runtime を閉じる。
        assert!(frame(12).contains("Switch"));
        assert!(!frame(12).contains("Open terminal"));
    }

    #[test]
    fn workspace_text_overlays_keep_home_visible_and_capture_scroll_keys() {
        let keys = [
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

        assert!(frame(1).contains("Preview"));
        assert!(frame(1).contains("session: overlays-session"));
        assert!(frame(1).contains("overlays-session")); // Home background remains visible.
        assert!(frame(4).contains("Diff"));
        assert!(frame(4).contains("unavailable until a backend"));
        assert!(frame(7).contains("Notes"));
        assert!(frame(7).contains("No notes are available"));
        assert!(frame(10).contains("Pull Request"));
        assert!(frame(10).contains("#42"));
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
            (vec![Key::Escape], Key::Escape),
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

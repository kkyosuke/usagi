//! Workspace 画面（ホーム）。
//!
//! workspace を開いている間の主画面。全幅の **header** の下を 2 ペインに割る:
//!
//! - 左ペイン **session menu** — セッション一覧（session）・root 行（root）・キー操作の footer。
//! - 右ペイン **closeup** — フォーカス中セッションの header・タブ切替の tabmenu・content・footer。
//!
//! 状態 [`Workspace`] は core の workspace と永続化済み [`WorkspaceState`] から構築する、端末 IO を
//! 持たない純粋な値である。[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。

use std::path::{Path, PathBuf};

use usagi_core::domain::pullrequest::PrLink;
use usagi_core::domain::session::SessionRecord;
use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
use usagi_core::domain::workspace_state::WorkspaceState;

use crate::presentation::layouts::panes;
use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets;
use crate::usecase::application::controller::{
    AppState, Feedback, HomeMode, Selection, Target, TargetPhase,
};
use crate::usecase::application::pane::{
    PaneKind, PaneSelection, PaneState, PaneTab, TabSelection,
};
use usagi_core::domain::id::{SessionId, WorkspaceId};

/// 左ペイン（session menu）の希望表示幅。残りが右ペイン（closeup）になる。
const LEFT_WIDTH: usize = 28;
/// header・rule の 2 行を除いた本文（ペイン）領域の先頭からのオフセット。
const CHROME_ROWS: usize = 2;
/// The v1 sidebar rabbit occupies three stable rows above the footer.
const MASCOT_ROWS: usize = 3;

/// Home snapshot の session 表示情報。
///
/// `id` が selection / active と照合する唯一の identity である。`label` は表示専用で、
/// 同名・変更・並び替えがあっても target の同一性には使わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedSession {
    /// daemon / snapshot が与える stable session identity。
    pub id: SessionId,
    /// sidebar に表示する名前。
    pub label: String,
    /// sidebar に表示する起源などの補足。
    pub detail: String,
    /// session pane の cwd。
    pub cwd: PathBuf,
}

/// controller の Home state を描画可能な root / session / action row へ投影した値。
///
/// session の順番は controller snapshot の `SessionId` 順を使い、表示情報は ID で結合する。
/// そのため表示名や入力 `Vec` の index を identity として扱わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeProjection {
    workspace: WorkspaceId,
    workspace_name: String,
    root_cwd: PathBuf,
    sessions: Vec<ProjectedSession>,
    selected: Selection,
    active: Target,
    mode: HomeMode,
    active_phase: TargetPhase,
    feedback: Option<Feedback>,
    mascot_tick: u64,
    pane_tabs: Vec<HomePaneTab>,
}

/// Home の右ペインに投影する tab strip の 1 項目。
///
/// tab の identity / 選択は pane reducer が所有する。この型はその state を描画向けの安全な
/// label と選択フラグへ変換しただけの値である。
#[derive(Debug, Clone, PartialEq, Eq)]
struct HomePaneTab {
    label: String,
    selected: bool,
}

impl HomeProjection {
    /// `state` を snapshot 表示情報へ安全に結合する。
    ///
    /// state にある ID だけをその順番で採用する。欠損した表示情報は描画せず、controller
    /// 側の snapshot reconciliation が selected / active を root に縮退させる。
    #[must_use]
    #[coverage(off)]
    pub fn from_state(
        state: &AppState,
        workspace_name: impl Into<String>,
        root_cwd: impl Into<PathBuf>,
        snapshot_sessions: &[ProjectedSession],
    ) -> Self {
        let sessions = state
            .sessions()
            .iter()
            .filter_map(|id| snapshot_sessions.iter().find(|session| session.id == *id))
            .cloned()
            .collect();
        Self {
            workspace: state.workspace(),
            workspace_name: workspace_name.into(),
            root_cwd: root_cwd.into(),
            sessions,
            selected: state.selected(),
            active: state.active(),
            mode: match state.route() {
                crate::usecase::application::controller::Route::Home(mode) => mode,
            },
            active_phase: state.phase_for(state.active()),
            feedback: state.feedback().cloned(),
            mascot_tick: state.mascot_tick(),
            pane_tabs: Vec::new(),
        }
    }

    /// pane reducer の tab と stable selection を右ペインへ投影する。
    ///
    /// pending/live の identity は reducer に残し、表示層は identity を文字列や index に
    /// 置換して操作しない。同名 tab も選択状態は `TabSelection` で区別される。
    #[must_use]
    #[coverage(off)]
    pub fn with_pane(mut self, pane: &PaneState) -> Self {
        self.pane_tabs = pane
            .tabs()
            .iter()
            .map(|tab| HomePaneTab {
                label: pane_tab_label(tab),
                selected: pane_tab_selected(tab, pane.selected()),
            })
            .collect();
        self
    }

    /// 左 sidebar の rows。root と `+ new session` は session 数にかかわらず常設する。
    #[must_use]
    #[coverage(off)]
    pub fn rows(&self) -> Vec<Selection> {
        let mut rows = Vec::with_capacity(self.sessions.len() + 2);
        rows.push(Selection::Target(Target::Root(self.workspace)));
        rows.extend(
            self.sessions
                .iter()
                .map(|session| Selection::Target(Target::Session(session.id))),
        );
        rows.push(Selection::NewSession);
        rows
    }

    #[coverage(off)]
    fn active_cwd(&self) -> &Path {
        match self.active {
            Target::Root(id) if id == self.workspace => &self.root_cwd,
            Target::Session(id) => self
                .sessions
                .iter()
                .find(|session| session.id == id)
                .map_or(self.root_cwd.as_path(), |session| session.cwd.as_path()),
            Target::Root(_) => &self.root_cwd,
        }
    }

    #[coverage(off)]
    fn active_label(&self) -> &str {
        match self.active {
            Target::Root(id) if id == self.workspace => "root",
            Target::Session(id) => self
                .sessions
                .iter()
                .find(|session| session.id == id)
                .map_or("root", |session| session.label.as_str()),
            Target::Root(_) => "root",
        }
    }
}

#[coverage(off)]
fn pane_tab_label(tab: &PaneTab) -> String {
    match tab {
        PaneTab::Pending(pending) => match pending.kind {
            PaneKind::Terminal => "Terminal (resolving)".to_owned(),
            PaneKind::Agent => "Agent (starting)".to_owned(),
        },
        PaneTab::Live(live) => match live.kind {
            PaneKind::Terminal => "Terminal".to_owned(),
            PaneKind::Agent => "Agent".to_owned(),
        },
    }
}

#[coverage(off)]
fn pane_tab_selected(tab: &PaneTab, selection: &PaneSelection) -> bool {
    match (tab, selection) {
        (PaneTab::Pending(pending), PaneSelection::Tab(TabSelection::Pending(selected))) => {
            pending.operation == *selected
        }
        (PaneTab::Live(live), PaneSelection::Tab(TabSelection::Live(selected))) => {
            live.terminal == *selected
        }
        (PaneTab::Pending(_) | PaneTab::Live(_), PaneSelection::Target(_))
        | (PaneTab::Pending(_), PaneSelection::Tab(TabSelection::Live(_)))
        | (PaneTab::Live(_), PaneSelection::Tab(TabSelection::Pending(_))) => false,
    }
}

/// Workspace 画面でキーボードが操作する対象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// セッション一覧から操作対象を選ぶ。
    Switch,
    /// 選択中セッションのタブやアクションを操作する。
    Closeup,
}

impl Mode {
    const ALL: [Self; 2] = [Self::Switch, Self::Closeup];

    #[coverage(off)]
    fn label(self) -> &'static str {
        match self {
            Self::Switch => "Switch",
            Self::Closeup => "Closeup",
        }
    }
}

/// 右ペインの 1 タブ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tab {
    /// タブのラベル。
    pub label: &'static str,
}

/// Workspace 画面の状態。左ペインは root 行を先頭に、[`WorkspaceState`] のセッション群を
/// 選択できる。右ペインのタブは Switch / Closeup のどちらでも切り替えられる。
#[derive(Debug, Clone)]
pub struct Workspace {
    record: WorkspaceRecord,
    state: WorkspaceState,
    mode: Mode,
    /// 選択行。`0` は root 行、`1..=sessions.len()` は session 行。
    selected: usize,
    tabs: Vec<Tab>,
    active_tab: usize,
}

impl Workspace {
    /// core の workspace とその永続化済み状態から画面状態を作る。
    #[must_use]
    #[coverage(off)]
    pub fn new(workspace: WorkspaceRecord, state: WorkspaceState) -> Self {
        Self {
            record: workspace,
            state,
            mode: Mode::Switch,
            selected: 0,
            tabs: vec![
                Tab { label: "Preview" },
                Tab { label: "Terminal" },
                Tab { label: "Diff" },
                Tab { label: "Notes" },
            ],
            active_tab: 0,
        }
    }

    /// workspace 名。
    #[must_use]
    #[coverage(off)]
    pub fn name(&self) -> &str {
        &self.record.name
    }

    /// workspace の絶対パス。
    #[must_use]
    #[coverage(off)]
    pub fn path(&self) -> &Path {
        &self.record.path
    }

    /// セッション一覧。
    #[must_use]
    #[coverage(off)]
    pub fn sessions(&self) -> &[SessionRecord] {
        &self.state.sessions
    }

    /// The workspace record passed to the daemon lifecycle command port.
    #[must_use]
    #[coverage(off)]
    pub fn record(&self) -> &WorkspaceRecord {
        &self.record
    }

    /// The selected session record, if the root row is not selected.
    #[must_use]
    #[coverage(off)]
    pub fn selected_session(&self) -> Option<&SessionRecord> {
        self.focused_session()
    }

    /// 現在の操作 mode。
    #[must_use]
    #[coverage(off)]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 選択中の session を操作する Closeup へ移る。
    ///
    /// session と tab の選択位置はそのまま維持する。
    #[coverage(off)]
    pub fn enter_closeup(&mut self) {
        self.mode = Mode::Closeup;
    }

    /// session 一覧を操作する Switch へ戻る。
    ///
    /// session と tab の選択位置はそのまま維持する。
    #[coverage(off)]
    pub fn enter_switch(&mut self) {
        self.mode = Mode::Switch;
    }

    /// application controller の Home mode を既存 view の表示 state に反映する。
    ///
    /// controller が selected / active の source of truth へ育つまで、既存 Workspace
    /// view の session・tab state はそのまま保持する最小の adapter である。
    #[coverage(off)]
    pub fn apply_home_mode(&mut self, mode: HomeMode) {
        self.mode = match mode {
            HomeMode::Switch => Mode::Switch,
            HomeMode::Closeup => Mode::Closeup,
        };
    }

    /// タブ一覧。
    #[must_use]
    #[coverage(off)]
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    /// 選択行の添字（`0` は root 行）。
    #[must_use]
    #[coverage(off)]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// アクティブなタブの添字。
    #[must_use]
    #[coverage(off)]
    pub fn active_tab(&self) -> usize {
        self.active_tab
    }

    /// root 行を選択しているか。
    #[must_use]
    #[coverage(off)]
    pub fn root_selected(&self) -> bool {
        self.selected == 0
    }

    /// フォーカス中 session の表示ラベル。root 行では `"root"`。
    #[must_use]
    #[coverage(off)]
    pub fn focused_label(&self) -> &str {
        self.focused_session()
            .map_or("root", SessionRecord::display_label)
    }

    /// フォーカス中 session に記録された Pull Request。root 行では空。
    #[must_use]
    #[coverage(off)]
    pub fn focused_prs(&self) -> &[PrLink] {
        self.focused_session()
            .map_or(&[], |session| session.prs.as_slice())
    }

    /// フォーカス中 target の preview に出す安全な概要行。
    #[must_use]
    #[coverage(off)]
    pub fn focused_preview_lines(&self) -> Vec<String> {
        let (kind, path) = self.focused_session().map_or_else(
            || ("workspace", self.path()),
            |session| ("session", session.root.as_path()),
        );
        vec![
            format!("{}: {}", kind, self.focused_label()),
            format!("path: {}", path.display()),
            format!("{} pull request(s)", self.focused_prs().len()),
        ]
    }

    /// フォーカス中 target の scratchpad を text overlay 用の安全な行へ投影する。
    #[must_use]
    #[coverage(off)]
    pub fn focused_note_lines(&self) -> Vec<String> {
        let notes = self
            .focused_session()
            .map_or(&self.state.root_notes, |session| &session.notes);
        let mut lines = Vec::new();
        if let Some(note) = notes.note() {
            lines.extend(note.lines().map(str::to_owned));
        }
        for todo in notes.todos() {
            lines.push(format!(
                "[{}] {}",
                if todo.done { 'x' } else { ' ' },
                todo.text
            ));
        }
        for decision in notes.decisions() {
            lines.push(format!(
                "{}  {}",
                decision.at.format("%Y-%m-%d"),
                decision.text
            ));
        }
        lines
    }

    /// 左ペインの選択を 1 つ下へ（末尾の session の次は先頭の root へ回り込む）。
    #[coverage(off)]
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1) % self.row_count();
    }

    /// 左ペインの選択を 1 つ上へ（先頭の root の次は末尾の session へ回り込む）。
    #[coverage(off)]
    pub fn select_prev(&mut self) {
        let rows = self.row_count();
        self.selected = (self.selected + rows - 1) % rows;
    }

    /// 右ペインのタブを次へ（末尾で先頭へ回り込む）。
    #[coverage(off)]
    pub fn tab_next(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
    }

    /// 右ペインのタブを前へ（先頭で末尾へ回り込む）。
    #[coverage(off)]
    pub fn tab_prev(&mut self) {
        let len = self.tabs.len();
        self.active_tab = (self.active_tab + len - 1) % len;
    }

    /// 選択できる行数（root 行 1＋セッション数）。
    #[coverage(off)]
    fn row_count(&self) -> usize {
        self.state.sessions.len() + 1
    }

    /// フォーカス中のセッション（root 選択なら `None`）。
    #[coverage(off)]
    fn focused_session(&self) -> Option<&SessionRecord> {
        self.selected
            .checked_sub(1)
            .and_then(|index| self.state.sessions.get(index))
    }
}

// ── header ──────────────────────────────────────────────────────────────────

/// 全幅の header: workspace 名のパンくずとセッション数。左寄せ・dim の区切り。
#[coverage(off)]
fn header_line(width: usize, ws: &Workspace) -> String {
    let count = ws.sessions().len();
    let sep = Style::new().dim().paint(" › ");
    let dot = Style::new().dim().paint(" · ");
    let modes = Mode::ALL
        .iter()
        .map(|mode| {
            if *mode == ws.mode() {
                Role::Accent.style().bold().paint(mode.label())
            } else {
                Style::new().dim().paint(mode.label())
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    let line = format!(
        " {}{sep}{}{dot}{}{dot}{modes}",
        Role::Success.style().bold().paint("USAGI"),
        Role::Success.style().bold().paint(ws.name()),
        Style::new().dim().paint(&format!("{count} sessions")),
    );
    widgets::pad_to_width(&line, width)
}

/// header と本文を分ける全幅の水平罫線（dim）。
#[coverage(off)]
fn rule_line(width: usize) -> String {
    Style::new().dim().paint(&"─".repeat(width))
}

// ── left pane: session menu ─────────────────────────────────────────────────

/// root 行。フォーカス中は強調する。
#[coverage(off)]
fn root_row(width: usize, ws: &Workspace) -> String {
    menu_row(width, ws.root_selected(), "root", "workspace root")
}

/// 選択可能な 1 行。`0` は root、`1..=sessions.len()` は session。
#[coverage(off)]
fn selectable_row(width: usize, ws: &Workspace, index: usize) -> String {
    if index == 0 {
        root_row(width, ws)
    } else {
        ws.sessions().get(index - 1).map_or_else(
            || root_row(width, ws),
            |session| {
                menu_row(
                    width,
                    index == ws.selected,
                    session.display_label(),
                    session.origin.as_str(),
                )
            },
        )
    }
}

/// `capacity` 行の viewport に選択行が必ず入るよう、先頭 index を決める。
#[coverage(off)]
fn viewport_start(selected: usize, row_count: usize, capacity: usize) -> usize {
    let visible = capacity.min(row_count);
    let max_start = row_count.saturating_sub(visible);
    selected
        .saturating_sub(visible.saturating_sub(1))
        .min(max_start)
}

/// 左ペインの 1 行: `>` カーソル＋名前（選択で accent 太字）＋dim の詳細。幅に詰める。
#[coverage(off)]
fn menu_row(width: usize, selected: bool, name: &str, detail: &str) -> String {
    let cursor = if selected {
        Role::Danger.style().bold().paint(">")
    } else {
        " ".to_string()
    };
    let name = if selected {
        Role::Accent.style().bold().paint(name)
    } else {
        name.to_string()
    };
    let detail = Style::new().dim().paint(detail);
    widgets::pad_to_width(&format!("{cursor} {name}  {detail}"), width)
}

/// 左ペインの footer（キー操作ヒント、dim）。
#[coverage(off)]
fn left_footer(width: usize, ws: &Workspace) -> String {
    let hint = match ws.mode() {
        Mode::Switch => "[switch] ↑↓ target",
        Mode::Closeup => "[closeup] target selected",
    };
    Style::new()
        .dim()
        .paint(&widgets::clip_to_width(hint, width))
}

/// 左ペイン（session menu）を `height` 行に組む。footer を最下行に
/// 固定し、残りを viewport として選択中の session / root 行を常に表示する。
#[coverage(off)]
fn left_pane(height: usize, width: usize, ws: &Workspace) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    if height == 1 {
        return vec![selectable_row(width, ws, ws.selected)];
    }

    let body_capacity = height - 1;
    let show_heading = body_capacity > 1;
    let viewport_capacity = body_capacity - usize::from(show_heading);
    let start = viewport_start(ws.selected, ws.row_count(), viewport_capacity);
    let end = (start + viewport_capacity).min(ws.row_count());

    let mut rows = Vec::with_capacity(height);
    if show_heading {
        rows.push(Role::Success.style().bold().paint("Sessions"));
    }
    for index in start..end {
        rows.push(selectable_row(width, ws, index));
    }
    rows.resize(body_capacity, String::new());
    rows.push(left_footer(width, ws));
    rows
}

// ── right pane: closeup ─────────────────────────────────────────────────────

/// closeup の header: フォーカス中セッションの identity と origin。root では workspace path。
#[coverage(off)]
fn closeup_header(width: usize, ws: &Workspace) -> String {
    let name = Role::Accent.style().bold().paint(ws.focused_label());
    let detail = ws.focused_session().map_or_else(
        || ws.path().display().to_string(),
        |session| format!("{} · {}", session.name, session.origin),
    );
    let detail = Style::new().dim().paint(&detail);
    widgets::pad_to_width(&format!(" {name}  {detail}"), width)
}

/// tabmenu: タブを並べ、アクティブを `[Label]` accent、他を dim で描く。
#[coverage(off)]
fn tab_menu(width: usize, ws: &Workspace) -> String {
    let tabs = ws
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            if i == ws.active_tab {
                format!("[{}]", Role::Accent.style().bold().paint(tab.label))
            } else {
                format!(" {} ", Style::new().dim().paint(tab.label))
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    widgets::pad_to_width(&format!(" {tabs}"), width)
}

/// content: アクティブなタブと、フォーカス中の実 workspace / session path。
#[coverage(off)]
fn content_lines(ws: &Workspace) -> Vec<String> {
    let tab = ws.tabs[ws.active_tab].label;
    let (kind, path) = ws.focused_session().map_or_else(
        || ("workspace", ws.path()),
        |session| ("session", session.root.as_path()),
    );
    vec![
        String::new(),
        Style::new()
            .dim()
            .paint(&format!("  {tab} — {kind} '{}'", ws.focused_label())),
        String::new(),
        Style::new().dim().paint(&format!("  {}", path.display())),
    ]
}

/// 右ペインの footer（キー操作ヒント、dim）。
#[coverage(off)]
fn right_footer(width: usize, ws: &Workspace) -> String {
    let hint = match ws.mode() {
        Mode::Switch => "←→/hl tab / Enter/t closeup / : commands / p PR / Esc back / q quit",
        Mode::Closeup => "←→/hl tab / ↑↓/jk action / : commands / p PR / Esc switch / q quit",
    };
    Style::new()
        .dim()
        .paint(&widgets::clip_to_width(hint, width))
}

/// 右ペイン（closeup）を `height` 行に組む: header・tabmenu・content、footer を最下行に固定。
#[coverage(off)]
fn right_pane(height: usize, width: usize, ws: &Workspace) -> Vec<String> {
    let mut rows = vec![
        closeup_header(width, ws),
        tab_menu(width, ws),
        String::new(),
    ];
    rows.extend(content_lines(ws));
    with_footer(rows, height, right_footer(width, ws))
}

// ── composition ─────────────────────────────────────────────────────────────

/// `rows` を `height` 行に収め、`footer` を最下行に固定する（本文が溢れたら切り、足りなければ
/// 空行で詰める）。
#[coverage(off)]
fn with_footer(mut rows: Vec<String>, height: usize, footer: String) -> Vec<String> {
    let body_cap = height.saturating_sub(1);
    rows.truncate(body_cap);
    rows.resize(body_cap, String::new());
    rows.push(footer);
    rows.truncate(height);
    rows
}

/// 生の端末サイズに対する workspace 画面 1 フレーム分の行。全幅の header と罫線の下を、共通の
/// [`panes`] レイアウトで左（session menu）・右（closeup）の 2 ペインに割って組む。サイズ 0 は
/// 80×24 にフォールバックする。
#[must_use]
#[coverage(off)]
pub fn render(raw_height: usize, raw_width: usize, ws: &Workspace) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    let mut frame = Vec::with_capacity(height);
    frame.push(header_line(width, ws));
    frame.push(rule_line(width));

    let body_height = height.saturating_sub(CHROME_ROWS);
    let split = panes::split(width, LEFT_WIDTH);
    let left = left_pane(body_height, split.left, ws);
    let right = right_pane(body_height, split.right, ws);
    frame.extend(panes::join(body_height, &left, &right, split));

    frame.truncate(height);
    frame
}

/// controller projection の Home frame を描く。
///
/// 既存 Workspace view と同じ header / 2-pane geometry / viewport を使う。左側の `>` は
/// navigation cursor、`*` は command target であり、異なる行でも同時に残る。
#[must_use]
#[coverage(off)]
pub fn render_home(raw_height: usize, raw_width: usize, home: &HomeProjection) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let split = panes::split(width, LEFT_WIDTH);
    let body_height = height.saturating_sub(CHROME_ROWS);
    let mut frame = Vec::with_capacity(height);
    frame.push(home_header_line(width, home));
    frame.push(rule_line(width));
    frame.extend(panes::join(
        body_height,
        &home_left_pane(body_height, split.left, home),
        &home_right_pane(body_height, split.right, home),
        split,
    ));
    frame.truncate(height);
    frame
}

#[coverage(off)]
fn home_header_line(width: usize, home: &HomeProjection) -> String {
    let mode = match home.mode {
        HomeMode::Switch => "Switch",
        HomeMode::Closeup => "Closeup",
    };
    widgets::pad_to_width(
        &format!(
            " {}{}{}{}{}{}",
            Role::Success.style().bold().paint("USAGI"),
            Style::new().dim().paint(" › "),
            Role::Success.style().bold().paint(&home.workspace_name),
            Style::new().dim().paint(" · "),
            Style::new()
                .dim()
                .paint(&format!("{} sessions", home.sessions.len())),
            Style::new().dim().paint(&format!(" · {mode}")),
        ),
        width,
    )
}

#[coverage(off)]
fn home_left_pane(height: usize, width: usize, home: &HomeProjection) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let rows = home.rows();
    if height == 1 {
        return vec![home_row(width, home, rows[0])];
    }
    let body_capacity = height - 1;
    let show_mascot = body_capacity >= MASCOT_ROWS + 2;
    let content_capacity = body_capacity - if show_mascot { MASCOT_ROWS } else { 0 };
    let show_heading = content_capacity > 1;
    let viewport_capacity = content_capacity - usize::from(show_heading);
    let selected_index = rows
        .iter()
        .position(|row| *row == home.selected)
        .unwrap_or(0);
    let start = viewport_start(selected_index, rows.len(), viewport_capacity);
    let end = (start + viewport_capacity).min(rows.len());
    let mut lines = Vec::with_capacity(height);
    if show_heading {
        lines.push(Role::Success.style().bold().paint("Sessions"));
    }
    lines.extend(
        rows[start..end]
            .iter()
            .map(|row| home_row(width, home, *row)),
    );
    lines.resize(content_capacity, String::new());
    if show_mascot {
        lines.extend(home_mascot(width, home.mascot_tick));
    }
    lines.push(Style::new().dim().paint(&widgets::clip_to_width(
        "[switch] ↑↓ cursor · Enter target",
        width,
    )));
    lines
}

/// The v1 sidebar mascot's resting browsing pose. It is deliberately a pure
/// function of the reducer-owned tick: the eyes blink and one ear twitches,
/// while every frame keeps the same three-row rectangle.
fn home_mascot(width: usize, tick: u64) -> [String; MASCOT_ROWS] {
    let phase = tick % 6;
    let ears = if phase == 5 { " (\\(/" } else { " (\\(\\" };
    let face = if phase == 4 { " (-.-)?" } else { " (o.o)?" };
    let feet = "o(_(\")(\")";
    [ears, face, feet].map(|line| {
        let left = width.saturating_sub(widgets::display_width(line)) / 2;
        Role::Feature.style().bold().paint(&widgets::pad_to_width(
            &format!("{}{}", " ".repeat(left), line),
            width,
        ))
    })
}

#[coverage(off)]
fn home_row(width: usize, home: &HomeProjection, row: Selection) -> String {
    let cursor = if home.selected == row {
        Role::Danger.style().bold().paint(">")
    } else {
        " ".to_string()
    };
    let target = match row {
        Selection::Target(target) => Some(target),
        Selection::NewSession => None,
    };
    let active = if target == Some(home.active) {
        Role::Accent.style().bold().paint("*")
    } else {
        " ".to_string()
    };
    let (label, detail) = match row {
        Selection::Target(Target::Root(_)) => ("root", "workspace root"),
        Selection::Target(Target::Session(id)) => home
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map_or(("root", "workspace root"), |session| {
                (session.label.as_str(), session.detail.as_str())
            }),
        Selection::NewSession => ("+ new session", "action"),
    };
    let label = if home.selected == row {
        Role::Accent.style().bold().paint(label)
    } else {
        label.to_string()
    };
    widgets::pad_to_width(
        &format!(
            "{cursor}{active} {label}  {}",
            Style::new().dim().paint(detail)
        ),
        width,
    )
}

#[coverage(off)]
fn home_right_pane(height: usize, width: usize, home: &HomeProjection) -> Vec<String> {
    let mode = match home.mode {
        HomeMode::Switch => "Switch",
        HomeMode::Closeup => "Closeup",
    };
    let header = widgets::pad_to_width(
        &format!(
            " {}  {}",
            Role::Accent.style().bold().paint(home.active_label()),
            Style::new().dim().paint("active target"),
        ),
        width,
    );
    let footer = Style::new().dim().paint(&widgets::clip_to_width(
        &format!("[{mode}] active pane"),
        width,
    ));
    if home.pane_tabs.is_empty() {
        let feedback = home
            .feedback
            .as_ref()
            .map(|feedback| feedback_label(Some(feedback)))
            .map(|message| format!("feedback: {message}"));
        let mut rows = vec![header];
        rows.extend(widgets::session_tab::empty_pane_with_detail(
            width,
            height.saturating_sub(2),
            "No tabs stirring yet. Enter starts one.",
            feedback.as_deref(),
        ));
        return with_footer(rows, height, footer);
    }

    let tabs = home
        .pane_tabs
        .iter()
        .map(|tab| widgets::session_tab::Tab {
            label: &tab.label,
            selected: tab.selected,
        })
        .collect::<Vec<_>>();
    let chrome = widgets::session_tab::render(width, &tabs);
    with_footer(
        vec![
            header,
            chrome[0].clone(),
            chrome[1].clone(),
            String::new(),
            Style::new()
                .dim()
                .paint(&format!("  cwd: {}", home.active_cwd().display())),
            String::new(),
            Style::new().dim().paint(&widgets::pad_to_width(
                &format!("  agent: {}", phase_label(home.active_phase)),
                width,
            )),
            Style::new().dim().paint(&widgets::pad_to_width(
                &format!("  feedback: {}", feedback_label(home.feedback.as_ref())),
                width,
            )),
        ],
        height,
        footer,
    )
}

#[coverage(off)]
fn phase_label(phase: TargetPhase) -> &'static str {
    match phase {
        TargetPhase::Absent => "absent",
        TargetPhase::Ready => "ready",
        TargetPhase::Running => "running",
        TargetPhase::Waiting => "waiting",
        TargetPhase::Done => "done",
    }
}

#[coverage(off)]
fn feedback_label(feedback: Option<&Feedback>) -> String {
    match feedback {
        None => "none".to_string(),
        Some(Feedback::Progress(message)) => format!("progress: {}", message.as_str()),
        Some(Feedback::OperationError(error)) => {
            format!(
                "operation error: {} ({})",
                error.message.as_str(),
                error.error_id
            )
        }
        Some(Feedback::TerminalError(error)) => {
            format!(
                "terminal error: {} ({})",
                error.message.as_str(),
                error.error_id
            )
        }
        Some(Feedback::Disconnected) => "disconnected; reconnect to continue".to_string(),
        Some(Feedback::Reconnected) => "reconnected; synchronizing state".to_string(),
        Some(Feedback::ResyncRequired) => "resync required; synchronizing state".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{HomeProjection, Mode, ProjectedSession, Workspace, render, render_home};
    use crate::presentation::widgets::{display_width, modal};
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, Feedback, HomeMode, SafeError, SafeMessage,
        Selection, Target, update,
    };
    use crate::usecase::application::pane::{
        PaneEvent, PaneKind, PaneSelection, PaneState, TabSelection, reduce,
    };
    use chrono::{DateTime, Utc};
    use std::path::PathBuf;
    use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::pullrequest::PrLink;
    use usagi_core::domain::session::{SessionOrigin, SessionRecord};
    use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
    use usagi_core::domain::workspace_state::WorkspaceState;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn session(name: &str, display_name: Option<&str>, origin: SessionOrigin) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: display_name.map(str::to_string),
            origin,
            started_from: None,
            root: PathBuf::from(format!("/tmp/actual/.usagi/sessions/{name}")),
            created_at: now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        }
    }

    fn workspace() -> Workspace {
        let record = WorkspaceRecord::new("actual", "/tmp/actual");
        let state = WorkspaceState {
            sessions: vec![
                session("tui", Some("UI work"), SessionOrigin::Human),
                session("daemon", None, SessionOrigin::Mcp),
            ],
            root_notes: Scratchpad::default(),
            updated_at: now(),
        };
        Workspace::new(record, state)
    }

    fn workspace_with_sessions(count: usize) -> Workspace {
        let record = WorkspaceRecord::new("actual", "/tmp/actual");
        let state = WorkspaceState {
            sessions: (0..count)
                .map(|index| session(&format!("session-{index:02}"), None, SessionOrigin::Human))
                .collect(),
            root_notes: Scratchpad::default(),
            updated_at: now(),
        };
        Workspace::new(record, state)
    }

    fn strip(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn joined(ws: &Workspace) -> String {
        render(30, 100, ws)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn projected_session(id: SessionId, label: &str, cwd: &str) -> ProjectedSession {
        ProjectedSession {
            id,
            label: label.to_string(),
            detail: "snapshot".to_string(),
            cwd: PathBuf::from(cwd),
        }
    }

    fn joined_home(home: &HomeProjection) -> String {
        render_home(30, 100, home)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn home_projection_keeps_root_sessions_and_new_in_identity_order() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let state = AppState::home(workspace, vec![second, first]);
        let snapshot = vec![
            projected_session(first, "same label", "/work/first"),
            projected_session(second, "same label", "/work/second"),
        ];
        let home = HomeProjection::from_state(&state, "work", "/work", &snapshot);

        assert_eq!(
            home.rows(),
            vec![
                Selection::Target(Target::Root(workspace)),
                Selection::Target(Target::Session(second)),
                Selection::Target(Target::Session(first)),
                Selection::NewSession,
            ]
        );
        let text = joined_home(&home);
        assert!(text.contains("root  workspace root"));
        assert_eq!(text.matches("same label  snapshot").count(), 2);
        assert!(text.contains("+ new session  action"));
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn home_projection_draws_selected_and_active_markers_on_different_rows() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let mut state = AppState::home(workspace, vec![first, second]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[
                projected_session(first, "first", "/work/first"),
                projected_session(second, "second", "/work/second"),
            ],
        );

        let lines = render_home(30, 100, &home)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line.contains(" * first")));
        assert!(lines.iter().any(|line| line.contains(">  second")));
        let text = joined_home(&home);
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn home_projection_never_marks_new_as_active_and_refresh_falls_back_to_root_cwd() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[projected_session(session, "session", "/work/session")],
        );
        let text = joined_home(&home);
        assert!(text.contains(">  + new session"));
        assert!(!text.contains("*> + new session"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(Vec::new())),
        );
        let refreshed = HomeProjection::from_state(&state, "work", "/work", &[]);
        // `+ new` は常設 action row のため refresh で消えない。一方、消えた active
        // session は typed identity で検出され root へ縮退する。
        assert_eq!(state.selected(), Selection::NewSession);
        assert_eq!(state.active(), Target::Root(workspace));
        assert!(joined_home(&refreshed).contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn home_projection_handles_tiny_geometry_and_an_unrelated_root_target_safely() {
        let workspace = WorkspaceId::new();
        let state = AppState::home(workspace, Vec::new());
        let mut home = HomeProjection::from_state(&state, "work", "/work", &[]);
        home.active = Target::Root(WorkspaceId::new());

        let zero_body = render_home(2, 20, &home);
        let one_row_body = render_home(3, 20, &home);
        assert_eq!(zero_body.len(), 2);
        assert_eq!(one_row_body.len(), 3);
        assert!(joined_home(&home).contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn home_sidebar_mascot_animates_only_on_tick_and_stays_in_the_background() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let initial = HomeProjection::from_state(&state, "work", "/work", &[]);
        let first = render_home(20, 80, &initial).join("\n");
        assert!(strip(&first).contains("(o.o)?"));

        for _ in 0..4 {
            let _ = update(&mut state, AppEvent::Tick);
        }
        let blink = HomeProjection::from_state(&state, "work", "/work", &[]);
        let blink_frame = render_home(20, 80, &blink).join("\n");
        assert_eq!(state.mascot_tick(), 4);
        assert!(strip(&blink_frame).contains("(-.-)?"));

        let narrow = render_home(8, 8, &blink);
        assert!(narrow.iter().all(|line| display_width(line) == 8));
    }

    #[test]
    #[coverage(off)]
    fn home_feedback_area_renders_safe_error_and_disconnect_without_raw_detail() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::OperationError(
                SafeError {
                    message: SafeMessage::new("Session creation failed"),
                    error_id: "err-safe-7".to_string(),
                },
            ))),
        );
        let home = HomeProjection::from_state(&state, "work", "/work", &[]);
        let text = joined_home(&home);
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
        assert!(text.contains("feedback: operation error: Session creation failed (err-safe-7)"));
        assert!(!text.contains("daemon internal detail: token=secret"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::Disconnected)),
        );
        let home = HomeProjection::from_state(&state, "work", "/work", &[]);
        let text = joined_home(&home);
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
        assert!(text.contains("feedback: disconnected; reconnect to continue"));
    }

    #[test]
    fn home_projection_renders_the_pane_reducer_tab_strip_and_selection() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let operation = OperationId::new();
        let mut pane = PaneState::new(PaneSelection::Target(target));
        let _ = reduce(
            &mut pane,
            PaneEvent::Request {
                operation,
                target,
                kind: PaneKind::Agent,
            },
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Pending(operation))),
        );
        let state = AppState::home(workspace, vec![session]);
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane);

        let text = joined_home(&home);
        assert!(text.contains(" Agent (starting) "));
        assert!(text.contains('▔'));
        assert!(!text.contains("No tabs stirring yet"));
    }

    #[test]
    #[coverage(off)]
    fn modal_composition_keeps_the_home_session_tab_as_its_background() {
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        let target = Target::Root(workspace);
        let mut pane = PaneState::new(PaneSelection::Target(target));
        let _ = reduce(
            &mut pane,
            PaneEvent::Request {
                operation,
                target,
                kind: PaneKind::Agent,
            },
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Pending(operation))),
        );
        let state = AppState::home(workspace, Vec::new());
        let home = HomeProjection::from_state(&state, "work", "/work", &[]).with_pane(&pane);
        let base = render_home(18, 100, &home);
        let over = modal::render_over(18, 100, &base, "Action", 20, &["modal".to_string()]);

        let plain = over.iter().map(|line| strip(line)).collect::<Vec<_>>();
        assert!(plain[3].contains("Agent (starting)"));
        assert!(plain[4].contains('▔'));
        assert!(plain.iter().any(|line| line.contains("┌─ Action")));
        assert!(over.iter().all(|line| display_width(line) == 100));
    }

    #[test]
    fn workspace_is_built_from_domain_records() {
        let ws = workspace();
        assert_eq!(ws.name(), "actual");
        assert_eq!(ws.path(), PathBuf::from("/tmp/actual"));
        assert_eq!(ws.sessions().len(), 2);
        assert_eq!(ws.sessions()[0].display_label(), "UI work");
        assert_eq!(ws.tabs().len(), 4);
        assert_eq!(ws.mode(), Mode::Switch);
        assert_eq!(ws.selected(), 0);
        assert_eq!(ws.active_tab(), 0);
        assert!(ws.root_selected());
        assert!(format!("{:?}", ws.clone()).contains("actual"));
        assert!(format!("{:?}", ws.tabs()[0]).contains("Preview"));
        assert_eq!(ws.tabs()[0], ws.tabs()[0]);
    }

    #[test]
    fn select_cycles_from_the_root_through_sessions() {
        let mut ws = workspace();
        ws.select_next();
        assert_eq!(ws.selected(), 1);
        ws.select_next();
        assert_eq!(ws.selected(), 2);
        ws.select_next();
        assert!(ws.root_selected());
        ws.select_prev();
        assert_eq!(ws.selected(), 2);
    }

    #[test]
    fn an_empty_workspace_selects_and_cycles_the_root_row() {
        let mut ws = Workspace::new(
            WorkspaceRecord::new("empty", "/tmp/empty"),
            WorkspaceState::new(),
        );
        assert!(ws.root_selected());
        ws.select_next();
        ws.select_prev();
        assert_eq!(ws.selected(), 0);
        let text = joined(&ws);
        assert!(text.contains("0 sessions"));
        assert!(text.contains("/tmp/empty"));
    }

    #[test]
    fn tab_navigation_wraps() {
        let mut ws = workspace();
        ws.tab_prev();
        assert_eq!(ws.active_tab(), 3);
        ws.tab_next();
        assert_eq!(ws.active_tab(), 0);
        ws.tab_next();
        assert_eq!(ws.active_tab(), 1);
        assert!(joined(&ws).contains("Terminal — workspace 'root'"));
    }

    #[test]
    fn mode_transitions_preserve_the_session_and_tab_selection() {
        let mut ws = workspace();
        ws.select_next();
        ws.tab_next();
        let selected = ws.selected();
        let active_tab = ws.active_tab();

        ws.enter_closeup();
        assert_eq!(ws.mode(), Mode::Closeup);
        assert_eq!(ws.selected(), selected);
        assert_eq!(ws.active_tab(), active_tab);

        ws.enter_switch();
        assert_eq!(ws.mode(), Mode::Switch);
        assert_eq!(ws.selected(), selected);
        assert_eq!(ws.active_tab(), active_tab);
        assert!(format!("{:?}", ws.mode()).contains("Switch"));
    }

    #[test]
    fn controller_mode_adapter_preserves_existing_view_selection() {
        let mut ws = workspace();
        ws.select_next();
        ws.tab_next();
        ws.apply_home_mode(HomeMode::Closeup);
        assert_eq!(ws.mode(), Mode::Closeup);
        assert_eq!(ws.selected(), 1);
        assert_eq!(ws.active_tab(), 1);
        ws.apply_home_mode(HomeMode::Switch);
        assert_eq!(ws.mode(), Mode::Switch);
    }

    #[test]
    fn focused_label_and_pull_requests_follow_the_selected_session() {
        let mut ws = workspace();
        ws.state.sessions[0]
            .prs
            .push(PrLink::new(42, "https://example.com/pull/42"));

        assert_eq!(ws.focused_label(), "root");
        assert!(ws.focused_prs().is_empty());

        ws.select_next();
        assert_eq!(ws.focused_label(), "UI work");
        assert_eq!(ws.focused_prs()[0].number, 42);

        ws.select_next();
        assert_eq!(ws.focused_label(), "daemon");
        assert!(ws.focused_prs().is_empty());
    }

    #[test]
    fn header_shows_both_modes_and_highlights_the_current_one() {
        let mut ws = workspace();
        let switch_header = &render(30, 100, &ws)[0];
        assert!(switch_header.contains("\u{1b}[1;36mSwitch\u{1b}[0m"));
        assert!(switch_header.contains("\u{1b}[2mCloseup\u{1b}[0m"));

        ws.enter_closeup();
        let closeup_header = &render(30, 100, &ws)[0];
        assert!(closeup_header.contains("\u{1b}[2mSwitch\u{1b}[0m"));
        assert!(closeup_header.contains("\u{1b}[1;36mCloseup\u{1b}[0m"));
    }

    #[test]
    fn render_uses_mode_specific_footers_and_keeps_tabs_visible() {
        let mut ws = workspace();
        let switch = joined(&ws);
        assert!(switch.contains("[switch] ↑↓ target"));
        assert!(switch.contains("←→/hl tab"));
        assert!(switch.contains("Enter/t closeup"));
        assert!(switch.contains("p PR"));
        for label in ["Preview", "Terminal", "Diff", "Notes"] {
            assert!(switch.contains(label));
        }

        ws.tab_next();
        ws.enter_closeup();
        let closeup_frame = render(30, 100, &ws);
        let closeup = closeup_frame
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(closeup.contains("[closeup] target selected"));
        assert!(closeup.contains("←→/hl tab"));
        assert!(closeup.contains("Esc switch"));
        assert!(closeup.contains("↑↓/jk action"));
        assert!(closeup.contains("Terminal — workspace 'root'"));
        assert!(
            closeup_frame
                .iter()
                .any(|line| line.contains("[\u{1b}[1;36mTerminal\u{1b}[0m]"))
        );
    }

    #[test]
    fn render_shows_real_workspace_and_session_records() {
        let text = joined(&workspace());
        assert!(text.contains("USAGI"));
        assert!(text.contains("actual"));
        assert!(text.contains("2 sessions"));
        assert!(text.contains("Sessions"));
        assert!(text.contains("UI work"));
        assert!(text.contains("human"));
        assert!(text.contains("daemon"));
        assert!(text.contains("mcp"));
        assert!(text.contains("Preview — workspace 'root'"));
        assert!(text.contains("/tmp/actual"));
        assert!(text.contains("root"));
        assert!(text.contains("Preview"));
        assert!(text.contains("Terminal"));
        assert!(text.contains("Esc back"));
        assert!(text.contains('│'));
    }

    #[test]
    fn render_places_the_selected_root_before_every_session() {
        let text = joined(&workspace());
        let root = text
            .find("> root  workspace root")
            .expect("selected root row");
        let first = text.find("UI work  human").expect("first session row");
        let second = text.find("daemon  mcp").expect("second session row");
        assert!(root < first);
        assert!(first < second);
    }

    #[test]
    fn render_reflects_selected_session_and_root() {
        let mut ws = workspace();
        let root_text = joined(&ws);
        assert!(root_text.contains("Preview — workspace 'root'"));
        assert!(root_text.contains("/tmp/actual"));

        ws.select_next();
        let session_text = joined(&ws);
        assert!(session_text.contains("tui · human"));
        assert!(session_text.contains("/tmp/actual/.usagi/sessions/tui"));

        ws.select_next();
        let second_session_text = joined(&ws);
        assert!(second_session_text.contains("daemon · mcp"));
        assert!(second_session_text.contains("/tmp/actual/.usagi/sessions/daemon"));
    }

    #[test]
    fn render_marks_only_one_selected_row() {
        let frame = render(30, 100, &workspace());
        let cursor_rows = frame
            .iter()
            .filter(|line| strip(line).trim_start().starts_with('>'))
            .count();
        assert_eq!(cursor_rows, 1);
    }

    #[test]
    fn session_viewport_keeps_every_selection_and_the_root_visible() {
        let mut ws = workspace_with_sessions(12);
        let tiny_frame = render(3, 100, &ws);
        assert!(
            tiny_frame
                .iter()
                .map(|line| strip(line))
                .any(|line| line.contains("> root"))
        );
        for expected in std::iter::once("root".to_string())
            .chain((0..12).map(|index| format!("session-{index:02}")))
        {
            let frame = render(8, 100, &ws);
            let selected = frame
                .iter()
                .map(|line| strip(line))
                .find(|line| line.trim_start().starts_with('>'))
                .expect("selected row must be inside the viewport");
            assert!(selected.contains(&expected), "selected row: {selected}");
            ws.select_next();
        }
    }

    #[test]
    fn render_fills_the_terminal_and_fits_its_width() {
        let frame = render(30, 100, &workspace());
        assert_eq!(frame.len(), 30);
        assert!(frame.iter().all(|line| display_width(line) == 100));
    }

    #[test]
    fn render_falls_back_for_a_zero_size() {
        let frame = render(0, 0, &workspace());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 80));
    }

    #[test]
    fn render_does_not_overflow_a_short_terminal() {
        assert_eq!(render(2, 80, &workspace()).len(), 2);
        assert_eq!(render(1, 80, &workspace()).len(), 1);
    }
}

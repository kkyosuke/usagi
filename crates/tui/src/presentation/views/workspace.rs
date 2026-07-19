//! Workspace 画面（ホーム）。
//!
//! workspace を開いている間の主画面。全幅の **header** の下を 2 ペインに割る:
//!
//! - 左ペイン **session menu** — セッション一覧（session）・root 行（root）・キー操作の footer。
//! - 右ペイン **closeup** — フォーカス中セッションの header・タブ切替の tabmenu・content・footer。
//!
//! 状態 [`Workspace`] は core の workspace と永続化済み [`WorkspaceState`] から構築する、端末 IO を
//! 持たない純粋な値である。[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use usagi_core::domain::pullrequest::PrLink;
use usagi_core::domain::session::SessionRecord;
use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
use usagi_core::domain::workspace_state::WorkspaceState;
use usagi_core::usecase::client::DaemonMetrics;

use crate::presentation::layouts::panes;
use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::views::closeup_modal::{self, CloseupModal};
use crate::presentation::views::decision_modal;
use crate::presentation::views::overview_modal::{self, OverviewModal};
use crate::presentation::views::pr_modal::{self, PrModal};
use crate::presentation::views::text_overlay::{self, OverlayDocument, TextOverlay};
use crate::presentation::widgets;
use crate::usecase::application::controller::{
    AppState, Feedback, HomeMode, PrOverlay, PreviewOverlay, Selection, Target, TargetPhase,
};
use crate::usecase::application::pane::{
    PaneKind, PaneSelection, PaneState, PaneTab, TabSelection,
};
use crate::usecase::application::terminal_selection::TerminalPoint;
use usagi_core::domain::id::{SessionId, WorkspaceId};

/// 左ペイン（session menu）の希望表示幅。ここだけを変更して sidebar 幅を調整する。
const LEFT_WIDTH: usize = 36;
/// header・rule の 2 行を除いた本文（ペイン）領域の先頭からのオフセット。
const CHROME_ROWS: usize = 2;
// The controller reducer owns the pointer hit-test and must resolve rows with
// the same sidebar geometry this view renders. Keep its mirrored constants in
// lock-step with the render so a click never lands on the wrong row.
const _: () = assert!(LEFT_WIDTH == crate::usecase::application::controller::SIDEBAR_LEFT_WIDTH);
const _: () = assert!(CHROME_ROWS == crate::usecase::application::controller::SIDEBAR_CHROME_ROWS);
/// v1 と同じ Nerd Font glyph: processor and resident-memory server.
const CPU_ICON: char = '\u{f2db}';
const MEMORY_ICON: char = '\u{f233}';
const MEBIBYTE: u64 = 1_048_576;
const GIBIBYTE: u64 = 1_073_741_824;

/// Returns the PTY viewport that is visible inside the right-hand pane.
#[must_use]
#[coverage(off)]
pub fn terminal_viewport(raw_height: usize, raw_width: usize) -> (usize, usize) {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let split = panes::split(width, LEFT_WIDTH);
    (
        // Header/tab chrome (3) plus the footer gap and footer (2) do not
        // display PTY cells. The PTY geometry must match the selectable output
        // viewport exactly, otherwise mouse rows drift as output scrolls.
        height.saturating_sub(CHROME_ROWS + 5).max(1),
        split.right.max(1),
    )
}

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
    /// daemon snapshot が与えた session の cwd。
    pub cwd: PathBuf,
    /// `last_active` がない旧 record では `created_at` を使う表示安全な更新時刻。
    pub last_modified: DateTime<Utc>,
    /// note scratchpad に表示できる内容があるか。icon の幅は常に予約する。
    pub has_notes: bool,
    /// dismissed を除いた PR の表示安全な要約。未解決 title は表示に要求しない。
    pub pr_summary: Option<String>,
    /// True while daemon-owned removal is pending.
    pub removing: bool,
}

/// Read-only Git facts supplied asynchronously by the composition layer.
///
/// A missing value means inspection has not completed or Git could not provide
/// a meaningful comparison; it is intentionally not rendered as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiff {
    pub base: String,
    pub ahead: usize,
    pub behind: usize,
    pub added: usize,
    pub removed: usize,
}

impl ProjectedSession {
    /// daemon snapshot record を、stable identity を保った sidebar projection へ変換する。
    #[must_use]
    #[coverage(off)]
    pub fn from_record(id: SessionId, record: &SessionRecord) -> Self {
        Self {
            id,
            label: record.display_label().to_owned(),
            detail: record.origin.as_str().to_owned(),
            cwd: record.root.clone(),
            last_modified: record.last_active_or_created(),
            has_notes: !record.notes.is_empty(),
            pr_summary: pr_summary(&record.prs),
            removing: false,
        }
    }
}

#[coverage(off)]
fn pr_summary(prs: &[PrLink]) -> Option<String> {
    let visible = prs.iter().filter(|pr| pr.is_visible()).collect::<Vec<_>>();
    let first = visible.first()?;
    let suffix = visible.len().saturating_sub(1);
    Some(if suffix == 0 {
        format!("PR #{}", first.number)
    } else {
        format!("PR #{} +{}", first.number, suffix)
    })
}

/// 選択中 live terminal を右ペインに描く presentation-only の投影素材。
///
/// 行データは runtime shell が daemon から poll し、scroll offset と feedback とともに
/// 毎フレーム投影入力として渡す。controller state（reducer）には持ち込まない。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalViewProjection {
    /// 選択中 live terminal tab の描画済み screen 行。
    pub rows: Vec<String>,
    /// viewport 下部に残す retained 行数。`0` は live 出力に追従する。
    pub scroll: usize,
    /// 端末操作に対する presentation-safe な feedback。footer に表示する。
    pub feedback: Option<String>,
}

/// controller の Home state を描画可能な root / session / action row へ投影した値。
///
/// session の順番は controller snapshot の `SessionId` 順を使い、表示情報は ID で結合する。
/// そのため表示名や入力 `Vec` の index を identity として扱わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeProjection {
    workspace: WorkspaceId,
    workspace_name: String,
    sessions: Vec<ProjectedSession>,
    selected: Selection,
    active: Target,
    mode: HomeMode,
    active_phase: TargetPhase,
    feedback: Option<Feedback>,
    mascot_tick: u64,
    /// Presentation-only message. Runtime state currently supplies `None`; this
    /// keeps a future event source out of the renderer and prevents dummy copy.
    mascot_speech: Option<widgets::mascot::MascotSpeech>,
    /// 最新の daemon observation。毎フレーム外部から与える描画素材で、controller
    /// state（reducer）には持たせない。`None` は metrics 導入前と同じ静かな mascot を保つ。
    metrics: Option<DaemonMetrics>,
    /// sidebar の git 差分列。stable `SessionId` で session 行に結合する非永続の描画素材で、
    /// controller state には持たせない。空なら差分列を描かず metrics 導入前の frame を保つ。
    git_diffs: BTreeMap<SessionId, GitDiff>,
    /// 選択中 live terminal の viewport 素材。`None` は live terminal 非表示で、右ペインは
    /// 既存の pane strip をそのまま描く。
    terminal_view: Option<TerminalViewProjection>,
    pane_tabs: Vec<HomePaneTab>,
    pane_error: Option<String>,
    closeup_action_visible: bool,
    decision_overlay: Option<crate::usecase::application::controller::DecisionOverlayState>,
    decisions: Vec<usagi_core::domain::user_decision::UserDecision>,
    /// Open Pull Request overlay projection, drawn above the sidebar/pane frame.
    pr_overlay: Option<PrOverlay>,
    /// Open Markdown preview overlay projection, drawn above the frame.
    preview_overlay: Option<PreviewOverlay>,
    /// Persisted Overview command-palette input, when its overlay is open. The
    /// runtime owns this so the caret and filter survive across frames.
    overview_modal: Option<OverviewModal>,
    /// Persisted Closeup action-modal input, when its overlay is open.
    closeup_modal: Option<CloseupModal>,
}

/// Home の右ペインに投影する tab strip の 1 項目。
///
/// tab の identity / 選択は pane reducer が所有する。この型はその state を描画向けの安全な
/// label と選択フラグへ変換しただけの値である。
#[derive(Debug, Clone, PartialEq, Eq)]
struct HomePaneTab {
    label: String,
    selected: bool,
    pending: bool,
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
        _root_cwd: impl Into<PathBuf>,
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
            sessions,
            selected: state.selected(),
            active: state.active(),
            mode: match state.route() {
                crate::usecase::application::controller::Route::Home(mode) => mode,
            },
            active_phase: state.phase_for(state.active()),
            feedback: state.feedback().cloned(),
            mascot_tick: state.mascot_tick(),
            mascot_speech: None,
            metrics: None,
            git_diffs: BTreeMap::new(),
            terminal_view: None,
            pane_tabs: Vec::new(),
            pane_error: None,
            closeup_action_visible: matches!(
                state.route(),
                crate::usecase::application::controller::Route::Home(HomeMode::Closeup)
            ) && (!state.has_live_pane()
                || state.overlay()
                    == Some(crate::usecase::application::controller::Overlay::Closeup)),
            decision_overlay: state.decision_overlay().cloned(),
            decisions: state.decisions().to_vec(),
            pr_overlay: state.pr_overlay().cloned(),
            preview_overlay: state.preview_overlay().cloned(),
            overview_modal: None,
            closeup_modal: None,
        }
    }

    /// Attach the runtime's persisted Overview / Closeup modal input so the
    /// overlay renders its live caret and selection instead of a rebuilt, empty
    /// modal. Both are `None` unless their overlay is open.
    #[must_use]
    pub fn with_overlay_modals(
        mut self,
        overview: Option<OverviewModal>,
        closeup: Option<CloseupModal>,
    ) -> Self {
        self.overview_modal = overview;
        self.closeup_modal = closeup;
        self
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
                pending: matches!(tab, PaneTab::Pending(_)),
            })
            .collect();
        self.pane_error = pane.error().map(str::to_owned);
        self
    }

    /// Attach a presentation-safe mascot message without changing controller or
    /// input state. `None` intentionally leaves the mascot silent.
    #[must_use]
    pub fn with_mascot_speech(mut self, speech: Option<widgets::mascot::MascotSpeech>) -> Self {
        self.mascot_speech = speech;
        self
    }

    /// Attach the latest daemon observation for the mascot sidecar without
    /// touching controller or input state. `None` leaves the sidecar empty so the
    /// home frame stays identical to its pre-metrics form.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Option<DaemonMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Attach the asynchronously refreshed Git observations drawn as the sidebar
    /// diff columns without touching controller or input state. The diffs join to
    /// session rows by stable `SessionId`; an empty map leaves the sidebar in its
    /// pre-diff form.
    #[must_use]
    pub fn with_git_diffs(mut self, diffs: &BTreeMap<SessionId, GitDiff>) -> Self {
        self.git_diffs = diffs.clone();
        self
    }

    /// Attach the focused live terminal's viewport rows, scroll offset and
    /// feedback for the right pane without touching controller or input state.
    /// `None` keeps the right pane on its existing tab strip.
    #[must_use]
    pub fn with_terminal_view(mut self, view: Option<TerminalViewProjection>) -> Self {
        self.terminal_view = view;
        self
    }

    /// 左 sidebar の rows。main と `+ new session` は session 数にかかわらず常設する。
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
    fn active_label(&self) -> &str {
        match self.active {
            Target::Root(id) if id == self.workspace => "main",
            Target::Session(id) => self
                .sessions
                .iter()
                .find(|session| session.id == id)
                .map_or("main", |session| session.label.as_str()),
            Target::Root(_) => "main",
        }
    }
}

#[coverage(off)]
fn pane_tab_label(tab: &PaneTab) -> String {
    match tab {
        PaneTab::Pending(pending) => match pending.kind {
            PaneKind::Terminal => "Terminal".to_owned(),
            PaneKind::Agent => "Agent".to_owned(),
            PaneKind::Diff => "Diff".to_owned(),
        },
        PaneTab::Live(live) => match live.kind {
            PaneKind::Terminal => "Terminal".to_owned(),
            PaneKind::Agent => "Agent".to_owned(),
            PaneKind::Diff => "Diff".to_owned(),
        },
        PaneTab::Ready(ready) => match ready.kind {
            PaneKind::Diff => "Diff".to_owned(),
            PaneKind::Terminal | PaneKind::Agent => "Pane".to_owned(),
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
        (PaneTab::Ready(ready), PaneSelection::Tab(TabSelection::Ready(selected))) => {
            ready.operation == *selected
        }
        (PaneTab::Pending(_) | PaneTab::Live(_) | PaneTab::Ready(_), PaneSelection::Target(_))
        | (
            PaneTab::Pending(_),
            PaneSelection::Tab(TabSelection::Live(_) | TabSelection::Ready(_)),
        )
        | (
            PaneTab::Live(_),
            PaneSelection::Tab(TabSelection::Pending(_) | TabSelection::Ready(_)),
        )
        | (
            PaneTab::Ready(_),
            PaneSelection::Tab(TabSelection::Pending(_) | TabSelection::Live(_)),
        ) => false,
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

    #[coverage(off)]
    fn icon(self) -> char {
        match self {
            Self::Switch => '\u{f0ec}',
            Self::Closeup => '\u{f00e}',
        }
    }
}
/// Daemon-authoritative session cache backing the controller Home projection.
///
/// Home row state, selection, input, and rendering live in the controller
/// (`AppState`/`render_home`); this view only holds the registry record, the
/// session records and their stable identities, and the non-persistent metrics
/// and Git observations the runtime refreshes each frame.
#[derive(Debug, Clone)]
pub struct Workspace {
    record: WorkspaceRecord,
    state: WorkspaceState,
    /// Stable daemon session identities, aligned with `state.sessions`.
    session_ids: Vec<SessionId>,
    /// 最新の daemon observation。永続 workspace state には保存しない。
    metrics: Option<DaemonMetrics>,
    /// Non-persistent, asynchronously refreshed Git observations by stable ID.
    git_diffs: BTreeMap<SessionId, GitDiff>,
}

impl Workspace {
    /// core の workspace とその永続化済み状態からセッションキャッシュを作る。
    #[must_use]
    #[coverage(off)]
    pub fn new(workspace: WorkspaceRecord, state: WorkspaceState) -> Self {
        let session_ids = state.sessions.iter().map(|_| SessionId::new()).collect();
        Self::with_runtime_ids(workspace, state, session_ids)
    }

    /// Build the cache from daemon-authoritative workspace state and session
    /// identities. The identities fence pane requests and completions.
    #[must_use]
    #[coverage(off)]
    pub fn with_runtime_ids(
        workspace: WorkspaceRecord,
        state: WorkspaceState,
        session_ids: Vec<SessionId>,
    ) -> Self {
        let session_ids = if session_ids.len() == state.sessions.len() {
            session_ids
        } else {
            state.sessions.iter().map(|_| SessionId::new()).collect()
        };
        Self {
            record: workspace,
            state,
            session_ids,
            metrics: None,
            git_diffs: BTreeMap::new(),
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

    /// Daemon identities aligned with [`Self::sessions`].
    #[must_use]
    #[coverage(off)]
    pub fn session_ids(&self) -> &[SessionId] {
        &self.session_ids
    }

    /// Replace only the sidebar's session projection from a daemon lifecycle
    /// snapshot. The persisted workspace state remains read-only auxiliary data.
    #[coverage(off)]
    pub fn replace_sessions(&mut self, sessions: Vec<SessionRecord>) {
        self.replace_sessions_and_ids(sessions, None);
    }

    /// Replace sidebar rows and their daemon-issued runtime identities from one
    /// lifecycle snapshot. The vectors are aligned by snapshot order; names
    /// remain display-only and are never used to recover an identity.
    #[coverage(off)]
    pub fn replace_sessions_with_runtime_ids(
        &mut self,
        sessions: Vec<SessionRecord>,
        session_ids: Vec<SessionId>,
    ) {
        self.replace_sessions_and_ids(sessions, Some(session_ids));
    }

    #[coverage(off)]
    fn replace_sessions_and_ids(
        &mut self,
        sessions: Vec<SessionRecord>,
        session_ids: Option<Vec<SessionId>>,
    ) {
        self.state.sessions = sessions;
        if let Some(session_ids) = session_ids {
            debug_assert_eq!(session_ids.len(), self.state.sessions.len());
            self.session_ids = session_ids;
        }
    }

    /// Replaces the daemon-observed metrics shown in the sidebar footer area.
    #[coverage(off)]
    pub fn set_metrics(&mut self, metrics: Option<DaemonMetrics>) {
        self.metrics = metrics;
    }

    /// The daemon metrics observation last stored by the runtime, for the
    /// controller `HomeProjection::with_metrics` projection.
    #[must_use]
    pub fn metrics(&self) -> Option<DaemonMetrics> {
        self.metrics.clone()
    }

    /// Replace the completed Git observations without blocking the renderer.
    #[coverage(off)]
    pub fn set_git_diffs(&mut self, diffs: BTreeMap<SessionId, GitDiff>) {
        self.git_diffs = diffs;
    }

    /// The completed Git observations keyed by session, for the controller
    /// `HomeProjection::with_git_diffs` projection.
    #[must_use]
    pub fn git_diffs(&self) -> &BTreeMap<SessionId, GitDiff> {
        &self.git_diffs
    }

    /// The workspace record passed to the daemon lifecycle command port.
    #[must_use]
    #[coverage(off)]
    pub fn record(&self) -> &WorkspaceRecord {
        &self.record
    }
}

// ── header ──────────────────────────────────────────────────────────────────

/// v1 の chrome と同じアイコン付き mode 表示。現在の mode だけを accent で強調する。
#[coverage(off)]
fn mode_toggle(current: Mode) -> String {
    Mode::ALL
        .iter()
        .map(|mode| {
            let label = format!("{} {}", mode.icon(), mode.label().to_ascii_lowercase());
            if *mode == current {
                Role::Accent.style().bold().paint(&label)
            } else {
                Style::new().dim().paint(&label)
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// 左の breadcrumb を必要な分だけ切り、mode toggle の右端位置を常に保つ。
#[coverage(off)]
fn header_with_mode_toggle(width: usize, left: &str, mode: Mode) -> String {
    let toggle = mode_toggle(mode);
    let toggle = widgets::clip_to_width(&toggle, width);
    let left_width = width.saturating_sub(widgets::display_width(&toggle));
    let left = widgets::clip_to_width(left, left_width);
    let gap = width
        .saturating_sub(widgets::display_width(&left))
        .saturating_sub(widgets::display_width(&toggle));
    format!("{left}{}{toggle}", " ".repeat(gap))
}

/// Header の下に呼吸できる余白を作る全幅の空行。
#[coverage(off)]
fn header_spacer(width: usize) -> String {
    " ".repeat(width)
}

// ── left pane: session menu ─────────────────────────────────────────────────

#[coverage(off)]
fn sidebar_divider(width: usize) -> String {
    // Indenting the rule gives the root row and the session group distinct
    // breathing room without moving the pane boundary itself.
    let indent = "  ".repeat(usize::from(width >= 2));
    let rule = Style::new()
        .dim()
        .paint(&"─".repeat(width.saturating_sub(widgets::display_width(&indent))));
    widgets::pad_to_width(&format!("{indent}{rule}"), width)
}

/// Git summary columns are sized once for the entire sidebar.  This keeps the
/// time, commit, and line-count cells at the same positions for every session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SidebarDiffColumns {
    ahead: usize,
    behind: usize,
    added: usize,
    removed: usize,
}

fn sidebar_diff_columns(
    session_ids: &[SessionId],
    diffs: &BTreeMap<SessionId, GitDiff>,
) -> SidebarDiffColumns {
    session_ids.iter().filter_map(|id| diffs.get(id)).fold(
        SidebarDiffColumns::default(),
        |columns, diff| SidebarDiffColumns {
            ahead: columns.ahead.max(decimal_digits(diff.ahead)),
            behind: columns.behind.max(decimal_digits(diff.behind)),
            added: columns.added.max(decimal_digits(diff.added)),
            removed: columns.removed.max(decimal_digits(diff.removed)),
        },
    )
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn sidebar_metadata(
    metadata: String,
    diff: Option<&GitDiff>,
    columns: SidebarDiffColumns,
    width: usize,
    dim: bool,
) -> String {
    if columns == SidebarDiffColumns::default() {
        return metadata;
    }
    let diff = diff.map_or_else(
        || " ".repeat(sidebar_git_summary_width(columns)),
        |diff| git_diff_text(diff, columns, dim),
    );
    let available = width.saturating_sub(2);
    let prefix = widgets::clip_to_width(&metadata, available);
    let gap = available
        .saturating_sub(widgets::display_width(&prefix))
        .saturating_sub(widgets::display_width(&diff));
    format!("{prefix}{}{diff}", " ".repeat(gap))
}

fn sidebar_git_summary_width(columns: SidebarDiffColumns) -> usize {
    let commits = usize::from(columns.ahead > 0) * (columns.ahead + 1)
        + usize::from(columns.ahead > 0 && columns.behind > 0)
        + usize::from(columns.behind > 0) * (columns.behind + 1);
    let lines = columns.added + columns.removed + 5;
    commits + lines + usize::from(commits > 0)
}

fn git_diff_text(diff: &GitDiff, columns: SidebarDiffColumns, dim: bool) -> String {
    let commit_style = |color| {
        let style = Style::new().fg(color);
        if dim { style.dim() } else { style }
    };
    let commits = match (columns.ahead > 0, columns.behind > 0) {
        (true, true) => format!(
            "{} {}",
            commit_style(Color::Cyan).paint(&format!(
                "↑{:>width$}",
                diff.ahead,
                width = columns.ahead
            )),
            commit_style(Color::Magenta).paint(&format!(
                "↓{:>width$}",
                diff.behind,
                width = columns.behind
            )),
        ),
        (true, false) => commit_style(Color::Cyan).paint(&format!(
            "↑{:>width$}",
            diff.ahead,
            width = columns.ahead
        )),
        (false, true) => commit_style(Color::Magenta).paint(&format!(
            "↓{:>width$}",
            diff.behind,
            width = columns.behind
        )),
        (false, false) => String::new(),
    };
    let success = if dim {
        Role::Success.style().dim()
    } else {
        Role::Success.style()
    };
    let danger = if dim {
        Role::Danger.style().dim()
    } else {
        Role::Danger.style()
    };
    let lines = format!(
        "{} {}",
        success.paint(&format!("+ {:>added$}", diff.added, added = columns.added)),
        danger.paint(&format!(
            "- {:>removed$}",
            diff.removed,
            removed = columns.removed
        )),
    );
    if commits.is_empty() {
        lines
    } else {
        format!("{commits} {lines}")
    }
}

#[coverage(off)]
fn mascot_metrics(metrics: Option<&DaemonMetrics>, frame: usize) -> Vec<String> {
    metrics.map_or_else(
        || {
            // Replace exactly one character in the status text while sweeping;
            // this keeps the label's layout stable instead of appending a rail.
            let waiting = widgets::shimmer_text_with(
                "waiting daemon",
                frame,
                widgets::Shimmer {
                    style: Style::new().fg(Color::White).bold(),
                    base_style: Style::new().fg(Color::White).dim(),
                    speed: 5,
                },
            );
            vec![waiting]
        },
        |metrics| {
            let cpu_label = format!(
                "{CPU_ICON} {:<4}",
                format!("{}%", metrics.cpu_percent_hundredths / 100)
            );
            let cpu = load_style(u64::from(metrics.cpu_percent_hundredths), 3_000, 12_000)
                .paint(&cpu_label);
            let memory = load_style(metrics.resident_memory_bytes, 512 * MEBIBYTE, 2 * GIBIBYTE)
                .paint(&format!(
                    "{MEMORY_ICON} {}",
                    format_memory(metrics.resident_memory_bytes)
                ));
            vec![format!("{cpu}  {memory}")]
        },
    )
}

#[coverage(off)]
fn load_style(value: u64, busy: u64, hot: u64) -> Style {
    if value >= hot {
        Style::new().fg(Color::Red)
    } else if value >= busy {
        Style::new().fg(Color::Yellow)
    } else {
        // The mascot row is pink. Set white explicitly so a calm metric does
        // not inherit that outer foreground colour before becoming dim.
        Style::new().fg(Color::White).dim()
    }
}

#[coverage(off)]
fn format_memory(bytes: u64) -> String {
    if bytes >= GIBIBYTE {
        let gibibytes = bytes / GIBIBYTE;
        let tenths = bytes % GIBIBYTE / 107_374_183;
        format!("{gibibytes}.{tenths}GB")
    } else {
        format!("{}MB", bytes / MEBIBYTE)
    }
}

// ── right pane: closeup ─────────────────────────────────────────────────────

// ── composition ─────────────────────────────────────────────────────────────

/// Pins a right-pane footer while preserving one blank breathing row above it.
/// Tiny terminals degrade to a footer-only row rather than overflowing.
#[coverage(off)]
fn with_footer_gap(mut rows: Vec<String>, height: usize, footer: String) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    if height == 1 {
        return vec![footer];
    }
    let body_cap = height - 2;
    rows.truncate(body_cap);
    rows.resize(body_cap, String::new());
    rows.push(String::new());
    rows.push(footer);
    rows
}

/// Retained live-terminal rows clipped into the right pane's content window.
///
/// Both the legacy `right_pane` and the controller `home_right_pane` share this so
/// the visible scrollback window (bottom-anchored, offset by `scroll`) is computed
/// identically on either render path.
fn terminal_viewport_rows(
    rows: &[String],
    scroll: usize,
    width: usize,
    content_cap: usize,
) -> Vec<String> {
    let start = rows
        .len()
        .saturating_sub(content_cap.saturating_add(scroll));
    rows.iter()
        .skip(start)
        .take(content_cap)
        .map(|line| widgets::clip_to_width(line, width))
        .collect()
}

/// The row count reserved above the live-terminal content inside the right pane:
/// the tab strip, the prefix line, and a blank spacer.
const RIGHT_PANE_CONTENT_TOP: usize = 3;
/// The footer gap reserved below the content window.
const RIGHT_PANE_FOOTER_GAP: usize = 2;

/// Convert a frame-cell pointer position into the retained-terminal viewport row
/// and column currently rendered in the right pane, or `None` when the pointer is
/// outside the live content window. `rows_len` and `scroll` describe the same
/// bottom-anchored window [`terminal_viewport_rows`] draws, so a drag maps back to
/// the exact cell under the cursor. This shares the pane geometry (chrome rows,
/// split, content window) with [`render_home`] rather than duplicating it.
#[must_use]
pub fn terminal_point_at(
    raw_height: usize,
    raw_width: usize,
    rows_len: usize,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TerminalPoint> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let split = panes::split(width, LEFT_WIDTH);
    // The divider occupies one column between the panes.
    let right_left = split.left.saturating_add(1);
    let column = usize::from(column).checked_sub(right_left)?;
    let body_row = usize::from(row).checked_sub(CHROME_ROWS)?;
    let content_row = body_row.checked_sub(RIGHT_PANE_CONTENT_TOP)?;
    let body_height = height.saturating_sub(CHROME_ROWS);
    let content_cap = body_height.saturating_sub(RIGHT_PANE_CONTENT_TOP + RIGHT_PANE_FOOTER_GAP);
    if content_row >= content_cap {
        return None;
    }
    let start = rows_len.saturating_sub(content_cap.saturating_add(scroll));
    Some(TerminalPoint {
        row: start + content_row,
        column,
    })
}

/// controller projection の Home frame を描く。
///
/// 既存 Workspace view と同じ header / 2-pane geometry / viewport を使う。左側の gutter は
/// navigation cursor と command target を stable [`Selection`] / [`Target`] identity から別々に
/// 投影する。Switch では cursor が優先し、Closeup では cursor を抑止して current marker を残す。
#[must_use]
#[coverage(off)]
pub fn render_home(raw_height: usize, raw_width: usize, home: &HomeProjection) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let split = panes::split(width, LEFT_WIDTH);
    let body_height = height.saturating_sub(CHROME_ROWS);
    let mut frame = Vec::with_capacity(height);
    frame.push(home_header_line(width, home));
    frame.push(header_spacer(width));
    let now = Utc::now();
    let right = dim_inactive_right_pane(
        home.mode == HomeMode::Switch,
        home_right_pane(body_height, split.right, home),
    );
    frame.extend(panes::join(
        body_height,
        &home_left_pane(body_height, split.left, home, now),
        &right,
        split,
    ));
    frame.truncate(height);
    if let Some(modal) = &home.overview_modal {
        overview_modal::render_over(height, width, &frame, modal)
    } else if let Some(overlay) = &home.pr_overlay {
        render_pr_overlay(height, width, &frame, overlay)
    } else if let Some(overlay) = &home.preview_overlay {
        render_preview_overlay(height, width, &frame, overlay)
    } else if let Some(overlay) = &home.decision_overlay {
        decision_modal::render_over(height, width, &frame, overlay, &home.decisions)
    } else if home.closeup_action_visible {
        // Prefer the runtime's persisted action modal (its caret and selection),
        // titled with the active target. Fall back to a fresh modal only for the
        // non-interactive snapshot path that has no runtime input state.
        let modal = home
            .closeup_modal
            .clone()
            .unwrap_or_else(|| CloseupModal::new(home.active_label()))
            .with_session(home.active_label());
        closeup_modal::render_over(height, width, &frame, &modal)
    } else {
        frame
    }
}

/// Compose the Pull Request overlay over `base`. A fetch error renders as a safe
/// unavailable notice; otherwise the list modal is drawn at its selection.
fn render_pr_overlay(
    height: usize,
    width: usize,
    base: &[String],
    overlay: &PrOverlay,
) -> Vec<String> {
    if let Some(error) = overlay.error() {
        return text_overlay::render_over(
            height,
            width,
            base,
            &TextOverlay::new(
                "Pull Request",
                OverlayDocument::Unavailable(error.message.as_str().to_owned()),
            ),
        );
    }
    pr_modal::render_over(
        height,
        width,
        base,
        &PrModal::with_selection(overlay.prs().to_vec(), overlay.selected()),
    )
}

/// Compose the Markdown preview overlay over `base`. A fetch error renders as a
/// safe unavailable notice; otherwise the preview lines are drawn at their scroll.
fn render_preview_overlay(
    height: usize,
    width: usize,
    base: &[String],
    overlay: &PreviewOverlay,
) -> Vec<String> {
    let document = overlay.error().map_or_else(
        || OverlayDocument::Ready(overlay.lines().to_vec()),
        |error| OverlayDocument::Unavailable(error.message.as_str().to_owned()),
    );
    text_overlay::render_over(
        height,
        width,
        base,
        &TextOverlay::new("Preview", document).scrolled_to(overlay.scroll()),
    )
}

/// Apply the inactive treatment only while the left sidebar owns navigation.
/// Modals are composed after this frame, preserving their foreground styles.
fn dim_inactive_right_pane(inactive: bool, right: Vec<String>) -> Vec<String> {
    if inactive {
        right
            .into_iter()
            .map(|line| widgets::dim_ansi(&line))
            .collect()
    } else {
        right
    }
}

#[coverage(off)]
fn home_header_line(width: usize, home: &HomeProjection) -> String {
    let mode = match home.mode {
        HomeMode::Switch => Mode::Switch,
        HomeMode::Closeup => Mode::Closeup,
    };
    let left = format!(
        " {}{}{}",
        Role::Success.style().bold().paint("USAGI"),
        Style::new().dim().paint(" > "),
        Role::Success.style().bold().paint(&home.workspace_name),
    );
    header_with_mode_toggle(width, &left, mode)
}

#[coverage(off)]
fn home_left_pane(
    height: usize,
    width: usize,
    home: &HomeProjection,
    now: DateTime<Utc>,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let rows = home.rows();
    // Size the Git summary columns once for the whole sidebar so every session's
    // commit and line cells align, matching the legacy `left_pane` computation.
    let session_ids = home
        .sessions
        .iter()
        .map(|session| session.id)
        .collect::<Vec<_>>();
    let columns = sidebar_diff_columns(&session_ids, &home.git_diffs);
    if height == 1 {
        return home_row_lines_at(width, home, rows[0], columns, now)
            .into_iter()
            .take(1)
            .collect();
    }
    let body_capacity = height - 1;
    // Reuse the legacy metric projection so both render paths draw an identical
    // sidecar. An absent observation yields no sidecar rows, which keeps the
    // pre-metrics home frame byte-for-byte unchanged.
    let metric_labels = home
        .metrics
        .as_ref()
        .map(|metrics| mascot_metrics(Some(metrics), 0))
        .unwrap_or_default();
    let mascot = widgets::mascot::sidebar_block_with_sidecar(
        width,
        home.mascot_tick,
        home.mascot_speech.as_ref(),
        &metric_labels,
    );
    let show_mascot = mascot
        .as_ref()
        .is_some_and(|block| body_capacity >= block.reserved_rows() + 2);
    let mascot_rows = if show_mascot {
        mascot
            .as_ref()
            .map_or(0, widgets::mascot::MascotBlock::reserved_rows)
    } else {
        0
    };
    let content_capacity = body_capacity.saturating_sub(mascot_rows);
    let viewport_capacity = content_capacity;
    let selected_index = rows
        .iter()
        .position(|row| *row == home.selected)
        .unwrap_or(0);
    let start = home_viewport_start(&rows, selected_index, viewport_capacity);
    let mut lines = Vec::with_capacity(height);
    for row in &rows[start..] {
        let row_lines = home_row_lines_at(width, home, *row, columns, now);
        if lines.len() + row_lines.len() > viewport_capacity {
            break;
        }
        lines.extend(row_lines);
        if matches!(row, Selection::Target(Target::Root(_))) && lines.len() < viewport_capacity {
            lines.push(sidebar_divider(width));
        }
    }
    lines.resize(content_capacity, String::new());
    if show_mascot {
        lines.extend(mascot.expect("shown mascot exists").rows().iter().cloned());
        lines.push(String::new());
    }
    let footer = match home.mode {
        HomeMode::Switch => "[switch] ↑↓ select / Enter closeup",
        HomeMode::Closeup => "[closeup] Ctrl-O then: o switch / a/Ctrl-A actions / n/p tabs",
    };
    lines.push(
        Style::new()
            .dim()
            .paint(&widgets::clip_to_width(footer, width)),
    );
    lines
}

#[coverage(off)]
fn home_viewport_start(rows: &[Selection], selected: usize, capacity: usize) -> usize {
    let mut start = 0;
    while start < selected
        && rows[start..=selected]
            .iter()
            .map(|row| home_row_height(*row))
            .sum::<usize>()
            > capacity
    {
        start += 1;
    }
    start
}

#[coverage(off)]
fn home_row_height(row: Selection) -> usize {
    if matches!(row, Selection::Target(Target::Root(_))) {
        2
    } else {
        usize::from(matches!(row, Selection::Target(Target::Session(_)))) + 1
    }
}

#[coverage(off)]
fn home_row_lines_at(
    width: usize,
    home: &HomeProjection,
    row: Selection,
    columns: SidebarDiffColumns,
    now: DateTime<Utc>,
) -> Vec<String> {
    let target = match row {
        Selection::Target(target) => Some(target),
        Selection::NewSession => None,
    };
    let (label, detail, session) = match row {
        Selection::Target(Target::Root(_)) => ("main", "workspace main", None),
        Selection::Target(Target::Session(id)) => home
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map_or(("main", "workspace main", None), |session| {
                (
                    session.label.as_str(),
                    session.detail.as_str(),
                    Some(session),
                )
            }),
        Selection::NewSession => ("+ new session", "", None),
    };
    let selected = home.mode == HomeMode::Switch && home.selected == row;
    if let Some(session) = session.filter(|session| session.removing) {
        let wave = widgets::Shimmer {
            style: Role::Danger.style().bold(),
            base_style: Role::Danger.style().dim(),
            speed: 4,
        };
        let frame = usize::try_from(home.mascot_tick).unwrap_or(usize::MAX);
        let label = widgets::shimmer_text_with(&session.label, frame, wave);
        return vec![
            widgets::pad_to_width(
                &format!("  {} {}", Role::Danger.style().bold().paint("✂"), label),
                width,
            ),
            String::new(),
        ];
    }
    let current = target == Some(home.active);
    let marker = home_row_marker(row, selected, current);
    let label = if session.is_some() {
        widgets::clip_to_width(label, width.saturating_sub(6))
    } else {
        label.to_string()
    };
    let label = if selected {
        Role::Accent.style().bold().paint(&label)
    } else if home.mode == HomeMode::Switch {
        // v1 keeps the Switch cursor legible by fading every inactive target.
        // Do this after the selected case so the cursor's established semantic
        // colour and marker precedence remain unchanged.
        Style::new().dim().paint(&label)
    } else if matches!(row, Selection::NewSession) {
        Role::Success.style().bold().paint(&label)
    } else if current {
        Role::Accent.style().bold().paint(&label)
    } else {
        Role::Accent.style().paint(&label)
    };
    let first = if let Some(session) = session {
        let note = if session.has_notes { "✎" } else { "·" };
        widgets::pad_to_width(
            &format!("{marker} {label}  {}", Style::new().dim().paint(note)),
            width,
        )
    } else {
        widgets::pad_to_width(
            &format!("{marker} {label}  {}", Style::new().dim().paint(detail)),
            width,
        )
    };
    if let Some(session) = session {
        let modified = widgets::relative_session_time(session.last_modified, now);
        let metadata = session.pr_summary.as_deref().map_or_else(
            || {
                format!(
                    "{} {modified}",
                    home_session_continuation_marker(selected, current)
                )
            },
            |pr| {
                format!(
                    "{} {modified} · {pr}",
                    home_session_continuation_marker(selected, current)
                )
            },
        );
        // Draw the same Git summary columns as the legacy sidebar. The whole
        // metadata row keeps Home's dim treatment; column widths reuse the shared
        // `sidebar_metadata` so both render paths align identically.
        let metadata = sidebar_metadata(
            metadata,
            home.git_diffs.get(&session.id),
            columns,
            width,
            true,
        );
        vec![
            first,
            widgets::pad_to_width(&Style::new().dim().paint(&metadata), width),
        ]
    } else {
        vec![first]
    }
}

/// v1-compatible sidebar marker with explicit precedence.
///
/// A selected session starts with v1's usagi glyph and uses a red `|` continuation;
/// in Closeup its active two-line stack is green. Root and action rows retain the compact
/// red `>` cursor in Switch.
#[coverage(off)]
fn home_row_marker(row: Selection, selected: bool, current: bool) -> String {
    if selected {
        return match row {
            Selection::Target(Target::Session(_)) => Role::Danger.style().bold().paint("\u{f0907}"),
            Selection::Target(Target::Root(_)) | Selection::NewSession => {
                Role::Danger.style().bold().paint(">")
            }
        };
    }
    if current {
        return Role::Success.style().bold().paint("|");
    }
    " ".to_string()
}

/// The second row of a session carries the same coloured rail as its identity row.
#[coverage(off)]
fn home_session_continuation_marker(selected: bool, current: bool) -> String {
    if selected {
        Role::Danger.style().bold().paint("|")
    } else if current {
        Role::Success.style().bold().paint("|")
    } else {
        " ".to_string()
    }
}

#[coverage(off)]
fn home_right_pane(height: usize, width: usize, home: &HomeProjection) -> Vec<String> {
    let mode = match home.mode {
        HomeMode::Switch => "Switch",
        HomeMode::Closeup => "Closeup",
    };
    let header = format!(
        " {}",
        Role::Accent.style().bold().paint(home.active_label())
    );
    let footer = Style::new().dim().paint(&widgets::clip_to_width(
        &format!("[{mode}] active pane"),
        width,
    ));
    if home.pane_tabs.is_empty() {
        let feedback = home
            .pane_error
            .as_deref()
            .map(str::to_owned)
            .or_else(|| {
                home.feedback
                    .as_ref()
                    .map(|feedback| feedback_label(Some(feedback)))
            })
            .map(|message| format!("feedback: {message}"));
        let mut rows = vec![header];
        rows.extend(widgets::session_tab::empty_pane_with_detail(
            width,
            height.saturating_sub(3),
            "No tabs stirring yet. Enter starts one.",
            feedback.as_deref(),
        ));
        return with_footer_gap(rows, height, footer);
    }

    let tabs = home
        .pane_tabs
        .iter()
        .map(|tab| widgets::session_tab::Tab {
            label: &tab.label,
            selected: tab.selected,
            pending_frame: tab.pending.then_some(home.mascot_tick),
        })
        .collect::<Vec<_>>();
    let chrome = widgets::session_tab::render_with_prefix(width, &header, &tabs);
    if let Some(view) = &home.terminal_view {
        // A focused live terminal renders daemon PTY output below the tab strip,
        // sharing the legacy viewport window and surfacing terminal feedback in
        // the footer.
        let mut rows = vec![chrome[0].clone(), chrome[1].clone(), String::new()];
        let content_cap = height.saturating_sub(rows.len() + 2);
        rows.extend(terminal_viewport_rows(
            &view.rows,
            view.scroll,
            width,
            content_cap,
        ));
        let footer = view.feedback.as_deref().map_or(footer, |feedback| {
            Style::new()
                .dim()
                .paint(&widgets::clip_to_width(feedback, width))
        });
        return with_footer_gap(rows, height, footer);
    }
    with_footer_gap(
        vec![
            chrome[0].clone(),
            chrome[1].clone(),
            String::new(),
            Style::new().dim().paint(&widgets::pad_to_width(
                &format!("  agent: {}", phase_label(home.active_phase)),
                width,
            )),
            Style::new().dim().paint(&widgets::pad_to_width(
                &format!(
                    "  feedback: {}",
                    home.pane_error
                        .as_deref()
                        .map_or_else(|| feedback_label(home.feedback.as_ref()), str::to_owned)
                ),
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
    use super::{
        CHROME_ROWS, GitDiff, HomeProjection, LEFT_WIDTH, ProjectedSession, TerminalViewProjection,
        Workspace, render_home, terminal_point_at, with_footer_gap,
    };
    use crate::presentation::widgets::mascot::MascotSpeech;
    use crate::presentation::widgets::{display_width, modal};
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, Feedback, HomeMode, Route, SafeError,
        SafeMessage, Selection, Target, update,
    };
    use crate::usecase::application::pane::{
        PaneEvent, PaneKind, PaneSelection, PaneState, PaneTab, TabSelection, reduce,
    };
    use crate::usecase::application::terminal_selection::TerminalPoint;

    use chrono::{DateTime, Utc};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use usagi_core::domain::id::{
        DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::pullrequest::{PrLink, PrState};

    use usagi_core::domain::session::{SessionOrigin, SessionRecord};

    use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
    use usagi_core::domain::workspace_state::WorkspaceState;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn terminal_point_at_maps_the_bottom_anchored_content_window() {
        // 20x80: split.left = 36, right pane starts at column 37 (after the
        // divider); the content window is 13 rows tall starting at frame row 5.
        // With 30 rows and no scroll it is anchored at retained row 17.
        assert_eq!(
            terminal_point_at(20, 80, 30, 0, 41, 5),
            Some(TerminalPoint { row: 17, column: 4 })
        );
        // Scrolling up shifts the anchored window toward older output.
        assert_eq!(
            terminal_point_at(20, 80, 30, 3, 37, 5),
            Some(TerminalPoint { row: 14, column: 0 })
        );
        // The last visible content row is selectable.
        assert_eq!(
            terminal_point_at(20, 80, 30, 0, 37, 17),
            Some(TerminalPoint { row: 29, column: 0 })
        );
    }

    #[test]
    fn terminal_point_at_rejects_pointers_outside_the_content_window() {
        // Left of the right pane, in the header chrome, just above the content,
        // and below the content window.
        assert_eq!(terminal_point_at(20, 80, 30, 0, 36, 5), None);
        assert_eq!(terminal_point_at(20, 80, 30, 0, 41, 1), None);
        assert_eq!(terminal_point_at(20, 80, 30, 0, 41, 4), None);
        assert_eq!(terminal_point_at(20, 80, 30, 0, 41, 18), None);
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

    #[test]
    fn right_pane_footer_keeps_a_blank_breathing_row() {
        let rows = with_footer_gap(vec!["body".to_string()], 4, "footer".to_string());
        assert_eq!(rows, vec!["body", "", "", "footer"]);
        assert_eq!(
            with_footer_gap(Vec::new(), 1, "footer".to_string()),
            vec!["footer"]
        );
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

    fn projected_session(id: SessionId, label: &str, cwd: &str) -> ProjectedSession {
        ProjectedSession {
            id,
            label: label.to_string(),
            detail: "snapshot".to_string(),
            cwd: PathBuf::from(cwd),
            last_modified: now(),
            has_notes: false,
            pr_summary: None,
            removing: false,
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
        assert!(text.contains("main  workspace main"));
        assert_eq!(text.matches("same label").count(), 2);
        assert!(text.contains("+ new session"));
        assert!(!text.contains("+ new session  action"));
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
    }

    fn pr_error() -> SafeError {
        SafeError {
            message: SafeMessage::new("gh unavailable"),
            error_id: "pr".into(),
        }
    }

    #[test]
    fn render_home_draws_the_pr_overlay_at_its_selection() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('p')));
        let mut first = PrLink::new(7, "https://github.com/o/r/pull/7");
        first.title = Some("add feature".into());
        let mut second = PrLink::new(8, "https://github.com/o/r/pull/8");
        second.title = Some("fix bug".into());
        second.state = PrState::Merged;
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target: Target::Root(workspace),
                prs: vec![first, second],
            }),
        );
        // Move the cursor to the second PR so the detail reflects the selection.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let home = HomeProjection::from_state(&state, "work", "/work", &[]);
        let text = joined_home(&home);
        assert!(text.contains("Pull Request"));
        assert!(text.contains("#7"));
        assert!(text.contains("add feature"));
        assert!(text.contains("merged"));
        // The selected PR's detail URL is the second one.
        assert!(text.contains("github.com/o/r/pull/8"));
    }

    #[test]
    fn render_home_draws_a_pr_fetch_error_as_a_safe_notice() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsError {
                target: Target::Root(workspace),
                error: pr_error(),
            }),
        );
        let home = HomeProjection::from_state(&state, "work", "/work", &[]);
        let text = joined_home(&home);
        assert!(text.contains("Pull Request"));
        assert!(text.contains("gh unavailable"));
    }

    #[test]
    fn render_home_draws_the_preview_overlay_and_its_error() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('v')));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target: Target::Root(workspace),
                lines: vec!["# Heading".into(), "content line".into()],
            }),
        );
        let ready = joined_home(&HomeProjection::from_state(&state, "work", "/work", &[]));
        assert!(ready.contains("Preview"));
        assert!(ready.contains("Heading"));
        assert!(ready.contains("content line"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewError {
                target: Target::Root(workspace),
                error: SafeError {
                    message: SafeMessage::new("no preview available"),
                    error_id: "preview".into(),
                },
            }),
        );
        let errored = joined_home(&HomeProjection::from_state(&state, "work", "/work", &[]));
        assert!(errored.contains("Preview"));
        assert!(errored.contains("no preview available"));
    }

    #[test]
    fn controller_mascot_reservation_matches_the_hit_test_constants() {
        // The controller pointer hit-test mirrors this view's foot-of-sidebar
        // mascot reservation with plain constants. The controller Home renders the
        // rabbit without a speech bubble, so pin those constants to what the mascot
        // widget actually reserves and where it drops out for width.
        use crate::presentation::widgets::mascot::sidebar_block_with_sidecar;
        use crate::usecase::application::controller::{
            SIDEBAR_MASCOT_MIN_LEFT, SIDEBAR_MASCOT_ROWS,
        };
        let block = sidebar_block_with_sidecar(LEFT_WIDTH, 0, None, &[])
            .expect("the rabbit fits the sidebar width");
        assert_eq!(block.reserved_rows(), SIDEBAR_MASCOT_ROWS);
        // Daemon metrics feed the sidecar beside the rabbit without adding rows, so
        // the reservation the hit-test assumes stays constant.
        let metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
        };
        let sidecar = super::mascot_metrics(Some(&metrics), 0);
        let with_metrics = sidebar_block_with_sidecar(LEFT_WIDTH, 0, None, &sidecar)
            .expect("the rabbit fits the sidebar width");
        assert_eq!(with_metrics.reserved_rows(), SIDEBAR_MASCOT_ROWS);
        // Just under the rabbit's footprint the mascot drops out entirely.
        assert!(sidebar_block_with_sidecar(SIDEBAR_MASCOT_MIN_LEFT - 1, 0, None, &[]).is_none());
        assert!(sidebar_block_with_sidecar(SIDEBAR_MASCOT_MIN_LEFT, 0, None, &[]).is_some());
    }

    #[test]
    fn home_projection_draws_selected_and_active_markers_on_different_rows() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let mut state = AppState::home(workspace, vec![first, second]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
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
        assert!(lines.iter().any(|line| line.contains("| first")));
        assert!(!lines.iter().any(|line| line.contains("\u{f0907} second")));
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
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[projected_session(session, "session", "/work/session")],
        );
        let text = joined_home(&home);
        assert!(!text.contains("> + new session"));
        assert!(!text.contains("| + new session"));

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
    fn home_projection_uses_v1_marker_precedence_and_hides_cursor_in_closeup() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let mut state = AppState::home(workspace, vec![first, second]);
        // Activate first, then move the cursor to second without changing the current target.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let snapshot = [
            projected_session(first, "同じ名前", "/work/first"),
            projected_session(second, "同じ名前", "/work/second"),
        ];

        let closeup = HomeProjection::from_state(&state, "work", "/work", &snapshot);
        let closeup_text = joined_home(&closeup);
        assert!(closeup_text.contains("| 同じ名前"));
        assert!(!closeup_text.contains("\u{f0907} 同じ名前"));
        assert!(closeup_text.contains("[closeup] Ctrl-O then"));
        let closeup_rendered = render_home(30, 100, &closeup).join("\n");
        assert!(closeup_rendered.contains("\u{1b}[1;36m同じ名前\u{1b}[0m"));
        assert!(closeup_rendered.contains("\u{1b}[36m同じ名前\u{1b}[0m"));

        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlO));
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        let switch = HomeProjection::from_state(&state, "work", "/work", &snapshot);
        let switch_text = joined_home(&switch);
        assert!(switch_text.contains("| 同じ名前"));
        assert!(switch_text.contains("\u{f0907} 同じ名前"));
        assert!(switch_text.contains("[switch] ↑↓ select"));

        for line in render_home(8, 7, &switch) {
            assert!(display_width(&line) <= 7);
        }
    }

    #[test]
    fn switch_dims_every_inactive_target_without_changing_selected_session_colour() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let mut state = AppState::home(workspace, vec![first, second]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlO));
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[
                projected_session(first, "first", "/work/first"),
                projected_session(second, "second", "/work/second"),
            ],
        );

        let rendered = render_home(30, 100, &home).join("\n");
        assert!(rendered.contains("\u{1b}[2mfirst\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[1;36msecond\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[2m+ new session\u{1b}[0m"));
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
    fn home_speech_reserves_a_blank_row_and_does_not_change_home_state() {
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let speech = MascotSpeech::new(["同期済み".to_owned()]).expect("speech");
        let home = HomeProjection::from_state(&state, "work", "/work", &[])
            .with_mascot_speech(Some(speech));
        let frame = render_home(30, 80, &home);
        let left_rows = frame[CHROME_ROWS..]
            .iter()
            .map(|line| strip(line).chars().take(LEFT_WIDTH).collect::<String>())
            .collect::<Vec<_>>();
        let bottom = left_rows
            .iter()
            .position(|line| line.contains("╰──┬"))
            .expect("bubble tail");
        assert!(left_rows[bottom + 2].contains("(o.o)?"));
        assert!(
            left_rows[bottom + 4].trim().is_empty(),
            "reserved blank row"
        );
        assert!(left_rows[bottom + 5].contains("[switch]"));
        assert_eq!(home.selected, state.selected());
        assert_eq!(home.active, state.active());
    }

    #[test]
    fn metrics_and_git_diff_getters_return_the_stored_projections() {
        let mut ws = workspace();
        assert!(ws.metrics().is_none());
        assert!(ws.git_diffs().is_empty());

        let metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 1,
            cpu_percent_hundredths: 0,
            resident_memory_bytes: 0,
            active_subscribers: 1,
            dropped_updates: 0,
        };
        ws.set_metrics(Some(metrics.clone()));
        assert_eq!(ws.metrics(), Some(metrics));

        let session = SessionId::new();
        let diff = GitDiff {
            base: "origin/main".into(),
            ahead: 1,
            behind: 0,
            added: 2,
            removed: 1,
        };
        ws.set_git_diffs(BTreeMap::from([(session, diff.clone())]));
        assert_eq!(ws.git_diffs().get(&session), Some(&diff));
    }

    #[test]
    fn home_metrics_sidecar_renders_the_daemon_metrics_row() {
        let metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
        };

        // The daemon observation flows through `with_metrics` into the sidecar row
        // beside usagi.
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let home = HomeProjection::from_state(&state, "actual", "/tmp/actual", &[])
            .with_metrics(Some(metrics));
        let controller = render_home(30, 100, &home);

        let controller_row = controller
            .iter()
            .find(|line| line.contains('\u{f2db}'))
            .expect("daemon metric row beside usagi");

        // The row carries both glyphs and the v1 CPU/memory summary text.
        assert!(strip(controller_row).contains("\u{f2db} 1%    \u{f233} 45MB"));
    }

    #[test]
    fn home_without_metrics_keeps_the_pre_metrics_frame() {
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let home = HomeProjection::from_state(&state, "work", "/work", &[]);
        let baseline = render_home(30, 100, &home);

        // Attaching an absent observation is a no-op on the rendered frame.
        let with_none = home.clone().with_metrics(None);
        assert_eq!(render_home(30, 100, &with_none), baseline);
        assert!(
            !baseline.iter().any(|line| line.contains('\u{f2db}')),
            "no daemon metric row without an observation"
        );
        assert!(strip(&baseline.join("\n")).contains("(o.o)?"));
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
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane);

        let text = joined_home(&home);
        assert!(text.contains("Agent"));
        assert!(text.contains('▔'));
        assert!(!text.contains("No tabs stirring yet"));
        assert!(!text.contains("/work/session"));

        let frame = render_home(30, 100, &home);
        let right_header = strip(&frame[CHROME_ROWS]);
        let name = right_header.find("session").expect("session name");
        let tab = right_header.find("Agent").expect("agent tab");
        assert!(name < tab);
    }

    #[test]
    fn home_right_pane_is_dim_in_switch_and_bright_in_closeup() {
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
        let state = AppState::home(workspace, Vec::new());
        let switch = HomeProjection::from_state(&state, "work", "/work", &[]).with_pane(&pane);
        let switch_frame = render_home(18, 100, &switch);
        let switch_right = switch_frame[CHROME_ROWS]
            .split_once('│')
            .expect("pane divider")
            .1;
        assert!(switch_right.contains("\u{1b}[2m"));
        assert!(switch_right.contains("\u{1b}[2;36mmain"));
        assert!(!switch_right.contains("\u{1b}[1;36m"));

        let mut state = state;
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let closeup = HomeProjection::from_state(&state, "work", "/work", &[]).with_pane(&pane);
        let closeup_frame = render_home(18, 100, &closeup);
        let closeup_right = closeup_frame[CHROME_ROWS]
            .split_once('│')
            .expect("pane divider")
            .1;
        assert!(closeup_right.contains("\u{1b}[1;36mmain"));
        assert!(!closeup_right.starts_with("\u{1b}[2m"));
    }

    #[test]
    fn pending_tab_chip_animates_on_home_tick_without_changing_the_pending_transition() {
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
        let mut state = AppState::home(workspace, vec![session]);
        let before = render_home(
            18,
            100,
            &HomeProjection::from_state(
                &state,
                "work",
                "/work",
                &[projected_session(session, "session", "/work/session")],
            )
            .with_pane(&pane),
        )
        .join("\n");
        for _ in 0..12 {
            let _ = update(&mut state, AppEvent::Tick);
        }
        let after = render_home(
            18,
            100,
            &HomeProjection::from_state(
                &state,
                "work",
                "/work",
                &[projected_session(session, "session", "/work/session")],
            )
            .with_pane(&pane),
        )
        .join("\n");
        assert_ne!(before, after);
        assert!(matches!(pane.tabs(), [PaneTab::Pending(_)]));
    }

    #[test]
    fn home_projection_renders_safe_agent_launch_failure_from_the_pane() {
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
            PaneEvent::Failed {
                operation,
                message: "agent launch is unavailable".to_owned(),
            },
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
        assert!(text.contains("feedback: agent launch is unavailable"));
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
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
        assert!(plain[2].contains("Agent"));
        assert!(plain[3].contains('▔'));
        assert!(plain.iter().any(|line| line.contains("┌─ Action")));
        assert!(over.iter().all(|line| display_width(line) == 100));
    }

    #[test]
    fn git_summary_supports_every_commit_column_shape() {
        let diff = GitDiff {
            base: "origin/main".to_owned(),
            ahead: 12,
            behind: 3,
            added: 1,
            removed: 2,
        };
        assert_eq!(super::decimal_digits(1_234), 4);

        let ahead_only = super::git_diff_text(
            &diff,
            super::SidebarDiffColumns {
                ahead: 2,
                behind: 0,
                added: 1,
                removed: 1,
            },
            false,
        );
        assert_eq!(strip(&ahead_only), "↑12 + 1 - 2");

        let behind_only = super::git_diff_text(
            &diff,
            super::SidebarDiffColumns {
                ahead: 0,
                behind: 1,
                added: 1,
                removed: 1,
            },
            false,
        );
        assert_eq!(strip(&behind_only), "↓3 + 1 - 2");

        let no_commits = super::git_diff_text(
            &diff,
            super::SidebarDiffColumns {
                ahead: 0,
                behind: 0,
                added: 1,
                removed: 1,
            },
            false,
        );
        assert_eq!(strip(&no_commits), "+ 1 - 2");
    }

    fn terminal_ref(workspace: WorkspaceId, session: SessionId) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        }
    }

    #[test]
    fn home_sidebar_git_columns_render_the_diff_row() {
        let diff = GitDiff {
            base: "origin/main".to_owned(),
            ahead: 3,
            behind: 2,
            added: 8,
            removed: 1,
        };

        // The observation flows through `with_git_diffs`, keyed by the stable
        // session id, into the sidebar commit-summary column.
        let workspace_id = WorkspaceId::new();
        let tui = SessionId::new();
        let daemon = SessionId::new();
        let state = AppState::home(workspace_id, vec![tui, daemon]);
        let home = HomeProjection::from_state(
            &state,
            "actual",
            "/tmp/actual",
            &[
                projected_session(tui, "UI work", "/work/tui"),
                projected_session(daemon, "daemon", "/work/daemon"),
            ],
        )
        .with_git_diffs(&BTreeMap::from([(daemon, diff)]));
        let controller = render_home(30, 100, &home);

        let diff_row = controller
            .iter()
            .map(|line| strip(line))
            .find(|line| line.contains("↑3 ↓2"))
            .expect("git diff row");
        assert!(diff_row.contains("↑3 ↓2 + 8 - 1"));
    }

    #[test]
    fn home_without_git_diffs_keeps_the_pre_diff_frame() {
        let workspace_id = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace_id, vec![session]);
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[projected_session(session, "session", "/work/session")],
        );
        let baseline = render_home(30, 100, &home);

        // Attaching an empty map is a no-op on the rendered frame.
        let with_empty = home.with_git_diffs(&BTreeMap::new());
        assert_eq!(render_home(30, 100, &with_empty), baseline);
        // No commit summary column is drawn without an observation.
        assert!(!baseline.iter().any(|line| strip(line).contains("↑0")));
    }

    #[test]
    fn home_right_pane_renders_live_terminal_viewport_and_feedback() {
        let workspace_id = WorkspaceId::new();
        let session = SessionId::new();
        let view_rows = vec![
            "old row".to_owned(),
            "middle row".to_owned(),
            "live row".to_owned(),
        ];

        // A focused live terminal's rows and feedback flow through
        // `with_terminal_view` into the right pane.
        let target = Target::Session(session);
        let terminal = terminal_ref(workspace_id, session);
        let mut pane = PaneState::new(PaneSelection::Target(target));
        let operation = OperationId::new();
        let _ = reduce(
            &mut pane,
            PaneEvent::Request {
                operation,
                target,
                kind: PaneKind::Terminal,
            },
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::Succeeded {
                operation,
                terminal: terminal.clone(),
            },
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(terminal))),
        );
        let mut state = AppState::home(workspace_id, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let home = HomeProjection::from_state(
            &state,
            "actual",
            "/tmp/actual",
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane)
        .with_terminal_view(Some(TerminalViewProjection {
            rows: view_rows,
            scroll: 0,
            feedback: Some("copied 3 lines".to_owned()),
        }));
        let controller = render_home(30, 100, &home);

        // Each frame line joins both panes; isolate the right pane past the
        // divider so the differing sidebar rows do not enter the comparison.
        let right_pane = |frame: &[String]| {
            frame
                .iter()
                .filter_map(|line| {
                    strip(line)
                        .split_once('│')
                        .map(|(_, right)| right.trim_end().to_owned())
                })
                .collect::<Vec<_>>()
        };
        let controller_right = right_pane(&controller);
        // The live viewport rows and the terminal feedback both appear in the pane.
        assert!(controller_right.iter().any(|line| line == "live row"));
        assert!(controller_right.iter().any(|line| line == "old row"));
        // The terminal feedback surfaces in the right-pane footer.
        assert!(
            controller
                .iter()
                .any(|line| strip(line).contains("copied 3 lines"))
        );
        // The viewport window keeps the newest row anchored to the bottom of the
        // content area.
        let bottom_output = controller_right
            .iter()
            .rfind(|line| line.ends_with(" row"))
            .cloned()
            .expect("a rendered output row");
        assert_eq!(bottom_output, "live row");
    }

    #[test]
    fn home_terminal_scroll_offset_matches_the_legacy_window() {
        let workspace_id = WorkspaceId::new();
        let session = SessionId::new();
        let rows = (0..20).map(|row| format!("row {row}")).collect::<Vec<_>>();

        let target = Target::Session(session);
        let terminal = terminal_ref(workspace_id, session);
        let mut pane = PaneState::new(PaneSelection::Target(target));
        let op = OperationId::new();
        let _ = reduce(
            &mut pane,
            PaneEvent::Request {
                operation: op,
                target,
                kind: PaneKind::Terminal,
            },
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::Succeeded {
                operation: op,
                terminal: terminal.clone(),
            },
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(terminal))),
        );
        let mut state = AppState::home(workspace_id, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let home = HomeProjection::from_state(
            &state,
            "actual",
            "/tmp/actual",
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane)
        .with_terminal_view(Some(TerminalViewProjection {
            rows,
            scroll: 2,
            feedback: None,
        }));
        let controller = render_home(24, 80, &home);

        let output_rows = |frame: &[String]| {
            frame
                .iter()
                .filter_map(|line| {
                    strip(line)
                        .split_once('│')
                        .map(|(_, right)| right.trim_end().to_owned())
                })
                .filter(|line| line.starts_with("row "))
                .collect::<Vec<_>>()
        };
        assert!(!output_rows(&controller).is_empty());
        // A two-row scrollback offset keeps the live tail hidden.
        assert!(!output_rows(&controller).iter().any(|line| line == "row 19"));
    }

    #[test]
    fn home_without_terminal_view_keeps_the_pane_strip() {
        let workspace_id = WorkspaceId::new();
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
        let mut state = AppState::home(workspace_id, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let home = HomeProjection::from_state(
            &state,
            "work",
            "/work",
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane);
        let baseline = render_home(30, 100, &home);

        // Attaching an absent terminal view leaves the agent tab strip untouched.
        let with_none = home.with_terminal_view(None);
        assert_eq!(render_home(30, 100, &with_none), baseline);
        assert!(
            strip(&baseline.join("\n")).contains("agent:"),
            "the pane strip stays without a live terminal view"
        );
    }

    #[test]
    fn waiting_daemon_sweep_advances_every_five_frames() {
        let rendered = super::mascot_metrics(None, 0).concat();
        let first = strip(&rendered);
        let held_rendered = super::mascot_metrics(None, 4).concat();
        let advanced_rendered = super::mascot_metrics(None, 5).concat();

        assert!(rendered.contains("\u{1b}[1;37mw\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[2;37ma\u{1b}[0m"));
        assert_eq!(rendered, held_rendered);
        assert_ne!(rendered, advanced_rendered);
        assert_eq!(first, strip(&advanced_rendered));
    }
}

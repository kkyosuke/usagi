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
use crate::presentation::views::closeup_modal::CloseupModal;
use crate::presentation::views::decision_modal;
use crate::presentation::widgets;
use crate::presentation::widgets::TextInput;
use crate::usecase::application::controller::{
    AppState, Feedback, HomeMode, Selection, Target, TargetPhase,
};
use crate::usecase::application::pane::{
    self, PaneEvent, PaneKind, PaneSelection, PaneState, PaneTab, TabSelection,
};
use crate::usecase::application::terminal_selection::TerminalPoint;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef, WorkspaceId};

/// 左ペイン（session menu）の希望表示幅。ここだけを変更して sidebar 幅を調整する。
const LEFT_WIDTH: usize = 36;
/// header・rule の 2 行を除いた本文（ペイン）領域の先頭からのオフセット。
const CHROME_ROWS: usize = 2;
/// v1 と同じ Nerd Font glyph: processor and resident-memory server.
const CPU_ICON: char = '\u{f2db}';
const MEMORY_ICON: char = '\u{f233}';
const MEBIBYTE: u64 = 1_048_576;
const GIBIBYTE: u64 = 1_073_741_824;

/// Returns the first screen column owned by the right pane.
#[must_use]
pub fn right_pane_left(raw_width: usize) -> usize {
    let (_, width) = widgets::normalize_size(0, raw_width);
    panes::split(width, LEFT_WIDTH).left.saturating_add(1)
}

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

    /// Hit-test a pointer at `(column, row)` against the sidebar and return the
    /// `Selection` it lands on, or `None` for the header, the divider under the
    /// Root row, the mascot sidecar, the footer, or a click outside the sidebar.
    ///
    /// This mirrors the `home_left_pane` layout (chrome rows, mascot reservation,
    /// viewport start, and the divider inserted after Root) so pointer selection
    /// shares the exact geometry the frame is drawn with. It is the controller
    /// replacement for the legacy `sidebar_row_at` index arithmetic.
    #[must_use]
    pub fn row_at(
        &self,
        raw_height: usize,
        raw_width: usize,
        column: u16,
        row: u16,
    ) -> Option<Selection> {
        let (height, width) = widgets::normalize_size(raw_height, raw_width);
        let split = panes::split(width, LEFT_WIDTH);
        if usize::from(column) >= split.left
            || usize::from(row) < CHROME_ROWS
            || height <= CHROME_ROWS
        {
            return None;
        }
        let rows = self.rows();
        let body_height = height - CHROME_ROWS;
        if body_height == 1 {
            return (usize::from(row) == CHROME_ROWS).then(|| rows[0]);
        }
        let body_capacity = body_height - 1;
        let content_capacity =
            body_capacity.saturating_sub(home_mascot_rows(self, split.left, body_capacity));
        let clicked = usize::from(row) - CHROME_ROWS;
        if clicked >= content_capacity {
            return None;
        }
        let selected_index = rows
            .iter()
            .position(|entry| *entry == self.selected)
            .unwrap_or(0);
        let start = home_viewport_start(&rows, selected_index, content_capacity);
        let mut offset = 0;
        for entry in &rows[start..] {
            let lines = home_row_content_lines(*entry);
            if offset + lines > content_capacity {
                break;
            }
            if (offset..offset + lines).contains(&clicked) {
                return Some(*entry);
            }
            offset += lines;
            if matches!(entry, Selection::Target(Target::Root(_))) && offset < content_capacity {
                if clicked == offset {
                    return None;
                }
                offset += 1;
            }
        }
        None
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
    /// 選択行。`0` は root 行、`1..=sessions.len()` は session 行、末尾は作成 action 行。
    selected: usize,
    pane_owner: WorkspaceId,
    /// Stable daemon session identities, aligned with `state.sessions`.
    session_ids: Vec<SessionId>,
    /// Pane state belongs to a selected target, never to the whole workspace.
    /// The legacy view still projects sessions by name, so the name is used only
    /// as the local map key; daemon operations retain their fenced identities.
    panes: BTreeMap<String, PaneState>,
    /// Completed read-only pane documents, keyed by their durable operation.
    pane_documents: BTreeMap<OperationId, Vec<String>>,
    /// The user-interaction counter captured when a pane launch was requested.
    /// A completion may take focus only while this remains unchanged, matching
    /// the session-create landing rule.
    pane_focus_at_request: BTreeMap<OperationId, u64>,
    interaction_count: u64,
    /// daemon で作成中の session。実 record が届くまで sidebar に skeleton として置く。
    pending_session: Option<String>,
    /// `+ new session` を置き換える v1-style inline name editor。
    create_input: Option<TextInput>,
    create_error: Option<String>,
    /// 最新の daemon observation。永続 workspace state には保存しない。
    metrics: Option<DaemonMetrics>,
    /// Non-persistent, asynchronously refreshed Git observations by stable ID.
    git_diffs: BTreeMap<SessionId, GitDiff>,
    /// 選択中 live terminal tab の描画済み screen 行。runtime が redraw ごとに
    /// daemon から poll した内容を投影する presentation-only の値。
    terminal_view: Option<Vec<String>>,
    /// Number of retained terminal rows kept below the viewport. Zero follows
    /// live output; this belongs to the selected target's presentation state.
    terminal_scroll: usize,
    terminal_feedback: Option<String>,
}

impl Workspace {
    /// core の workspace とその永続化済み状態から画面状態を作る。
    #[must_use]
    #[coverage(off)]
    pub fn new(workspace: WorkspaceRecord, state: WorkspaceState) -> Self {
        let session_ids = state.sessions.iter().map(|_| SessionId::new()).collect();
        Self::with_runtime_ids(workspace, state, WorkspaceId::new(), session_ids)
    }

    /// Build a workspace view from daemon-authoritative workspace and session
    /// identities. These identities fence pane requests and completions; names
    /// remain display-only map keys for the legacy view projection.
    #[must_use]
    #[coverage(off)]
    pub fn with_runtime_ids(
        workspace: WorkspaceRecord,
        state: WorkspaceState,
        pane_owner: WorkspaceId,
        session_ids: Vec<SessionId>,
    ) -> Self {
        let session_ids = if session_ids.len() == state.sessions.len() {
            session_ids
        } else {
            state.sessions.iter().map(|_| SessionId::new()).collect()
        };
        let mut panes = BTreeMap::from([(
            String::new(),
            PaneState::new(PaneSelection::Target(Target::Root(pane_owner))),
        )]);
        for session in &state.sessions {
            panes.insert(
                session.name.clone(),
                PaneState::new(PaneSelection::Target(Target::Root(pane_owner))),
            );
        }
        Self {
            record: workspace,
            state,
            mode: Mode::Switch,
            selected: 0,
            pane_owner,
            session_ids,
            panes,
            pane_documents: BTreeMap::new(),
            pane_focus_at_request: BTreeMap::new(),
            interaction_count: 0,
            pending_session: None,
            create_input: None,
            create_error: None,
            metrics: None,
            git_diffs: BTreeMap::new(),
            terminal_view: None,
            terminal_scroll: 0,
            terminal_feedback: None,
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
    /// snapshot.  The legacy workspace state remains read-only auxiliary data.
    /// A removed selected row safely falls back to root; a same-name recreated
    /// row is treated as the snapshot's current incarnation.
    #[coverage(off)]
    pub fn replace_sessions(&mut self, sessions: Vec<SessionRecord>) {
        self.replace_sessions_and_ids(sessions, None);
    }

    /// Replace sidebar rows and their daemon-issued runtime identities from
    /// one lifecycle snapshot.  The vectors are aligned by snapshot order;
    /// names remain display-only and are never used to recover an identity.
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
        let selected_name = self.focused_session().map(|session| session.name.clone());
        // `pane()` is a read-only projection used by every render. Keep its
        // per-session state map in lockstep with the daemon-owned snapshot before
        // exposing a newly-created row to selection; otherwise selecting that row
        // makes `pane()` look up a key that was never initialized and panics.
        self.panes.retain(|name, _| {
            name.is_empty() || sessions.iter().any(|session| session.name == *name)
        });
        for session in &sessions {
            self.panes.entry(session.name.clone()).or_insert_with(|| {
                PaneState::new(PaneSelection::Target(Target::Root(self.pane_owner)))
            });
        }
        self.state.sessions = sessions;
        if let Some(session_ids) = session_ids {
            debug_assert_eq!(session_ids.len(), self.state.sessions.len());
            self.session_ids = session_ids;
        }
        self.selected = selected_name
            .and_then(|name| {
                self.state
                    .sessions
                    .iter()
                    .position(|session| session.name == name)
            })
            .map_or(0, |index| index + 1);
    }

    /// 新しい session を作成する間、sidebar に非選択の skeleton 行を表示する。
    #[coverage(off)]
    pub fn begin_pending_session(&mut self, name: String) {
        self.pending_session = Some(name);
    }

    /// session 作成の skeleton を取り除く。
    #[coverage(off)]
    pub fn clear_pending_session(&mut self) {
        self.pending_session = None;
    }

    /// 作成中の session 名。skeleton の描画だけが利用する。
    #[must_use]
    #[coverage(off)]
    pub fn pending_session(&self) -> Option<&str> {
        self.pending_session.as_deref()
    }

    /// Replaces the daemon-observed metrics shown in the sidebar footer area.
    #[coverage(off)]
    pub fn set_metrics(&mut self, metrics: Option<DaemonMetrics>) {
        self.metrics = metrics;
    }

    /// Replace the completed Git observations without blocking the renderer.
    #[coverage(off)]
    pub fn set_git_diffs(&mut self, diffs: BTreeMap<SessionId, GitDiff>) {
        self.git_diffs = diffs;
    }

    fn git_diff(&self, index: usize) -> Option<&GitDiff> {
        self.session_ids
            .get(index)
            .and_then(|id| self.git_diffs.get(id))
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

    /// Stable daemon identity of the focused session, if a session is selected.
    #[must_use]
    #[coverage(off)]
    pub fn focused_session_id(&self) -> Option<SessionId> {
        self.selected
            .checked_sub(1)
            .and_then(|index| self.session_ids.get(index).copied())
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
        &[]
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
        0
    }

    /// root 行を選択しているか。
    #[must_use]
    #[coverage(off)]
    pub fn root_selected(&self) -> bool {
        self.selected == 0
    }

    /// `+ new session` action row が選択されているか。
    #[must_use]
    #[coverage(off)]
    pub fn new_session_selected(&self) -> bool {
        self.selected == self.state.sessions.len() + 1
    }

    /// Move the sidebar cursor to the persistent `+ new session` row.
    #[coverage(off)]
    pub fn select_new_session(&mut self) {
        self.selected = self.state.sessions.len() + 1;
    }

    /// Select a sidebar row resolved from a pointer hit test. Out-of-range
    /// indices are ignored so a stale frame can never select a different row.
    #[coverage(off)]
    pub fn select_row(&mut self, index: usize) {
        if index < self.row_count() {
            self.selected = index;
        }
    }

    #[must_use]
    #[coverage(off)]
    pub fn creating_session_inline(&self) -> bool {
        self.create_input.is_some()
    }

    #[must_use]
    #[coverage(off)]
    pub fn inline_create_value(&self) -> Option<&str> {
        self.create_input.as_ref().map(TextInput::value)
    }

    #[coverage(off)]
    pub fn begin_inline_session_create(&mut self, first: Option<char>) {
        let mut input = TextInput::default();
        if let Some(character) = first {
            input.insert(character);
        }
        self.create_input = Some(input);
        self.create_error = None;
    }

    #[coverage(off)]
    pub fn cancel_inline_session_create(&mut self) {
        self.create_input = None;
        self.create_error = None;
    }

    #[coverage(off)]
    pub fn inline_create_insert(&mut self, character: char) {
        if let Some(input) = &mut self.create_input {
            input.insert(character);
            self.create_error = None;
        }
    }

    #[coverage(off)]
    pub fn inline_create_backspace(&mut self) {
        if let Some(input) = &mut self.create_input {
            input.backspace();
            self.create_error = None;
        }
    }

    #[coverage(off)]
    pub fn inline_create_move(&mut self, right: bool) {
        if let Some(input) = &mut self.create_input {
            if right {
                input.move_right();
            } else {
                input.move_left();
            }
        }
    }

    pub fn inline_create_name(&mut self) -> Option<String> {
        let name = self.create_input.as_ref()?.value().trim();
        if name.is_empty() {
            self.create_error = Some("session name is required".to_owned());
            None
        } else {
            Some(name.to_owned())
        }
    }

    #[coverage(off)]
    pub fn fail_inline_session_create(&mut self, error: String) {
        self.create_error = Some(error);
    }

    #[coverage(off)]
    pub fn finish_inline_session_create(&mut self) {
        self.create_input = None;
        self.create_error = None;
    }

    /// フォーカス中 session の表示ラベル。main 行では `"main"`。
    #[must_use]
    #[coverage(off)]
    pub fn focused_label(&self) -> &str {
        self.focused_session()
            .map_or("main", SessionRecord::display_label)
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
        self.select_pane_tab(1);
    }

    /// 右ペインのタブを前へ（先頭で末尾へ回り込む）。
    #[coverage(off)]
    pub fn tab_prev(&mut self) {
        self.select_pane_tab(-1);
    }

    /// Records a user action. Runtime wakeups deliberately do not call this,
    /// so an asynchronous pane completion can safely land after redraws.
    #[coverage(off)]
    pub fn record_interaction(&mut self) {
        self.interaction_count = self.interaction_count.saturating_add(1);
    }

    /// Request a visible daemon-owned pane placeholder.
    /// The pane reducer keeps the durable operation identity until the runtime
    /// replaces it with its fenced terminal reference; focus waits for that
    /// completion unless the user acts in the meantime.
    #[coverage(off)]
    pub fn open_pane(&mut self, kind: PaneKind) -> usagi_core::domain::id::OperationId {
        let operation = usagi_core::domain::id::OperationId::new();
        let target = self.pane_target();
        let pane = self.pane_mut();
        let _ = pane::reduce(
            pane,
            PaneEvent::Request {
                operation,
                target,
                kind,
            },
        );
        self.pane_focus_at_request
            .insert(operation, self.interaction_count);
        operation
    }

    /// Apply a daemon-owned terminal completion to the currently selected pane.
    #[coverage(off)]
    pub fn complete_pane(
        &mut self,
        operation: usagi_core::domain::id::OperationId,
        terminal: usagi_core::domain::id::TerminalRef,
    ) {
        let _ = pane::reduce(
            self.pane_mut(),
            PaneEvent::Succeeded {
                operation,
                terminal: terminal.clone(),
            },
        );
        self.focus_completed_pane(operation, TabSelection::Live(terminal));
    }

    /// Complete a non-terminal pane and retain its safe document for rendering.
    #[coverage(off)]
    pub fn resolve_pane(&mut self, operation: OperationId, document: Vec<String>) {
        let pane = self.pane_mut();
        let _ = pane::reduce(pane, PaneEvent::Resolved { operation });
        if pane
            .tabs()
            .iter()
            .any(|tab| matches!(tab, PaneTab::Ready(ready) if ready.operation == operation))
        {
            self.pane_documents.insert(operation, document);
        }
        self.focus_completed_pane(operation, TabSelection::Ready(operation));
    }

    /// Remove a pending pane after a presentation-safe daemon error.
    #[coverage(off)]
    pub fn fail_pane(&mut self, operation: usagi_core::domain::id::OperationId, message: String) {
        self.pane_focus_at_request.remove(&operation);
        let _ = pane::reduce(self.pane_mut(), PaneEvent::Failed { operation, message });
    }

    /// Close the selected right-pane tab without affecting daemon ownership.
    #[coverage(off)]
    pub fn close_pane(&mut self) {
        let pending = match self.pane().selected() {
            PaneSelection::Tab(TabSelection::Pending(operation)) => Some(*operation),
            PaneSelection::Target(_)
            | PaneSelection::Tab(TabSelection::Live(_) | TabSelection::Ready(_)) => None,
        };
        let document = match self.pane().selected() {
            PaneSelection::Tab(TabSelection::Ready(operation)) => Some(*operation),
            PaneSelection::Target(_)
            | PaneSelection::Tab(TabSelection::Pending(_) | TabSelection::Live(_)) => None,
        };
        let _ = pane::reduce(self.pane_mut(), PaneEvent::CloseSelected);
        if let Some(operation) = pending {
            self.pane_focus_at_request.remove(&operation);
        }
        if let Some(operation) = document {
            self.pane_documents.remove(&operation);
        }
    }

    #[coverage(off)]
    fn focus_completed_pane(&mut self, operation: OperationId, selection: TabSelection) {
        let Some(accepted_at) = self.pane_focus_at_request.remove(&operation) else {
            return;
        };
        if accepted_at == self.interaction_count {
            let _ = pane::reduce(
                self.pane_mut(),
                PaneEvent::Select(PaneSelection::Tab(selection)),
            );
        }
    }

    /// The fenced terminal of the focused live tab, if the selection is one.
    ///
    /// The runtime uses this to attach, poll and route keystrokes to the
    /// daemon-owned terminal that is currently on screen.
    #[must_use]
    #[coverage(off)]
    pub fn focused_live_terminal(&self) -> Option<&TerminalRef> {
        match self.pane().selected() {
            PaneSelection::Tab(TabSelection::Live(terminal)) => Some(terminal),
            PaneSelection::Tab(TabSelection::Pending(_) | TabSelection::Ready(_))
            | PaneSelection::Target(_) => None,
        }
    }

    /// Projects the focused live terminal's rendered screen rows, or clears it.
    /// This is presentation-only state supplied by the runtime each redraw.
    #[coverage(off)]
    pub fn set_terminal_view(&mut self, rows: Option<Vec<String>>) {
        self.terminal_view = rows;
        let len = self.terminal_view.as_ref().map_or(0, Vec::len);
        self.terminal_scroll = self.terminal_scroll.min(len.saturating_sub(1));
    }

    /// Move one retained row toward older terminal output.
    #[coverage(off)]
    pub fn terminal_scroll_up(&mut self) {
        let len = self.terminal_view.as_ref().map_or(0, Vec::len);
        self.terminal_scroll = (self.terminal_scroll + 1).min(len.saturating_sub(1));
    }

    /// Move one retained row back toward the live terminal bottom.
    #[coverage(off)]
    pub fn terminal_scroll_down(&mut self) {
        self.terminal_scroll = self.terminal_scroll.saturating_sub(1);
    }

    #[coverage(off)]
    pub fn set_terminal_feedback(&mut self, feedback: Option<String>) {
        self.terminal_feedback = feedback;
    }

    #[coverage(off)]
    fn pane_target(&self) -> Target {
        self.selected
            .checked_sub(1)
            .and_then(|index| self.session_ids.get(index).copied())
            .map_or(Target::Root(self.pane_owner), Target::Session)
    }

    /// Pane state rendered by the right-hand Chrome strip.
    ///
    /// # Panics
    ///
    /// Panics when the selected target has no local pane state. Constructors
    /// and target selection maintain this invariant.
    #[must_use]
    #[coverage(off)]
    pub fn pane(&self) -> &PaneState {
        self.panes
            .get(&self.pane_key())
            .expect("a pane state exists for every selected target")
    }

    /// Safe document lines for the selected completed non-terminal tab.
    #[must_use]
    #[coverage(off)]
    pub fn pane_document(&self) -> Option<&[String]> {
        let PaneSelection::Tab(TabSelection::Ready(operation)) = self.pane().selected() else {
            return None;
        };
        self.pane_documents.get(operation).map(Vec::as_slice)
    }

    #[must_use]
    #[coverage(off)]
    pub fn has_panes(&self) -> bool {
        !self.pane().tabs().is_empty()
    }

    #[coverage(off)]
    fn select_pane_tab(&mut self, direction: i8) {
        let pane = self.pane_mut();
        let tabs = pane.tabs();
        if tabs.is_empty() {
            return;
        }
        let current = tabs
            .iter()
            .position(|tab| pane_tab_selected(tab, pane.selected()))
            .unwrap_or(0);
        let next = if direction > 0 {
            (current + 1) % tabs.len()
        } else {
            (current + tabs.len() - 1) % tabs.len()
        };
        let selection = match &tabs[next] {
            PaneTab::Pending(pending) => {
                PaneSelection::Tab(TabSelection::Pending(pending.operation))
            }
            PaneTab::Live(live) => PaneSelection::Tab(TabSelection::Live(live.terminal.clone())),
            PaneTab::Ready(ready) => PaneSelection::Tab(TabSelection::Ready(ready.operation)),
        };
        let _ = pane::reduce(pane, PaneEvent::Select(selection));
    }

    #[coverage(off)]
    fn pane_key(&self) -> String {
        self.focused_session()
            .map_or_else(String::new, |session| session.name.clone())
    }

    #[coverage(off)]
    fn pane_mut(&mut self) -> &mut PaneState {
        let key = self.pane_key();
        self.panes
            .entry(key)
            .or_insert_with(|| PaneState::new(PaneSelection::Target(Target::Root(self.pane_owner))))
    }

    /// 選択できる行数（root 行 1＋セッション数＋作成 action 行）。
    #[coverage(off)]
    fn row_count(&self) -> usize {
        self.state.sessions.len() + 2
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

/// 全幅の header: workspace 名のパンくずは左、mode toggle は右上に固定する。
#[coverage(off)]
fn header_line(width: usize, ws: &Workspace) -> String {
    let sep = Style::new().dim().paint(" > ");
    let left = format!(
        " {}{sep}{}",
        Role::Success.style().bold().paint("USAGI"),
        Role::Success.style().bold().paint(ws.name()),
    );
    header_with_mode_toggle(width, &left, ws.mode())
}

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

/// main は session と同じ2行の marker stack で描く。
#[coverage(off)]
fn root_rows(width: usize, ws: &Workspace) -> Vec<String> {
    let selected = ws.root_selected();
    let (marker, continuation) = if selected {
        match ws.mode() {
            Mode::Switch => (
                Role::Danger.style().bold().paint("\u{f0907}"),
                Role::Danger.style().bold().paint("|"),
            ),
            Mode::Closeup => (
                Role::Success.style().bold().paint("|"),
                Role::Success.style().bold().paint("|"),
            ),
        }
    } else {
        (" ".to_owned(), " ".to_owned())
    };
    let name = match (selected, ws.mode()) {
        (true, _) => Role::Accent.style().bold().paint("main"),
        (false, Mode::Switch) => Style::new().dim().paint("main"),
        (false, Mode::Closeup) => Role::Accent.style().paint("main"),
    };
    vec![
        widgets::pad_to_width(&format!("{marker} {name}"), width),
        widgets::pad_to_width(
            &format!(
                "{continuation} {}",
                Style::new().dim().paint("workspace main")
            ),
            width,
        ),
    ]
}

/// 選択可能な 1 行。`0` は root、`1..=sessions.len()` は session、末尾は作成 action。
#[coverage(off)]
fn selectable_rows(width: usize, ws: &Workspace, index: usize) -> Vec<String> {
    if index == 0 {
        root_rows(width, ws)
    } else if index == ws.sessions().len() + 1 {
        create_session_rows(width, index == ws.selected, ws)
    } else {
        ws.sessions().get(index - 1).map_or_else(
            || root_rows(width, ws),
            |session| {
                session_menu_rows(
                    width,
                    index == ws.selected,
                    ws.mode(),
                    session,
                    ws.git_diff(index - 1),
                    sidebar_diff_columns(ws.session_ids(), &ws.git_diffs),
                )
            },
        )
    }
}

#[coverage(off)]
fn workspace_row_height(index: usize, ws: &Workspace) -> usize {
    if index == ws.sessions().len() + 1 {
        1 + usize::from(ws.create_error.is_some()) + 2 * usize::from(ws.pending_session.is_some())
    } else {
        2
    }
}

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

#[coverage(off)]
fn workspace_viewport_start(selected: usize, ws: &Workspace, capacity: usize) -> usize {
    let mut start = 0;
    while start < selected
        && (start..=selected)
            .map(|index| workspace_row_height(index, ws))
            .sum::<usize>()
            > capacity
    {
        start += 1;
    }
    start.min(ws.row_count().saturating_sub(1))
}

/// Resolve a left-sidebar cell in the current frame to its selectable row.
///
/// The calculation deliberately follows [`left_pane`]'s viewport and pending
/// skeleton layout, so a click can neither land on the divider/mascot/footer
/// nor select a row hidden above the viewport.
#[must_use]
#[coverage(off)]
pub fn sidebar_row_at(
    raw_height: usize,
    raw_width: usize,
    ws: &Workspace,
    skeleton_frame: usize,
    column: u16,
    row: u16,
) -> Option<usize> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let split = panes::split(width, LEFT_WIDTH);
    if usize::from(column) >= split.left || usize::from(row) < CHROME_ROWS || height <= CHROME_ROWS
    {
        return None;
    }
    let body_height = height - CHROME_ROWS;
    if body_height == 1 {
        return (usize::from(row) == CHROME_ROWS).then_some(ws.selected);
    }
    let body_capacity = body_height - 1;
    let metric_labels = mascot_metrics(ws.metrics.as_ref(), skeleton_frame);
    let mascot = widgets::mascot::sidebar_block_with_sidecar(
        split.left,
        skeleton_frame as u64,
        None,
        &metric_labels,
    );
    let mascot_rows = mascot
        .as_ref()
        .filter(|block| body_capacity >= block.reserved_rows() + 2)
        .map_or(0, widgets::mascot::MascotBlock::reserved_rows);
    let content_capacity = body_capacity.saturating_sub(mascot_rows);
    let target = usize::from(row) - CHROME_ROWS;
    if target >= content_capacity {
        return None;
    }

    let start = workspace_viewport_start(ws.selected, ws, content_capacity);
    let mut offset = 0;
    for index in start..ws.row_count() {
        let pending_rows =
            usize::from(index == ws.sessions().len() + 1 && ws.pending_session.is_some()) * 2;
        let entry_rows = pending_rows + workspace_row_height(index, ws);
        if offset + entry_rows > content_capacity {
            break;
        }
        if target >= offset && target < offset + entry_rows {
            return (target >= offset + pending_rows).then_some(index);
        }
        offset += entry_rows;
        if index == 0 && offset < content_capacity {
            if target == offset {
                return None;
            }
            offset += 1;
        }
    }
    None
}

#[coverage(off)]
fn create_session_rows(width: usize, selected: bool, ws: &Workspace) -> Vec<String> {
    let cursor = if selected {
        Role::Danger.style().bold().paint(">")
    } else {
        " ".to_owned()
    };
    let style = if ws.mode() == Mode::Switch && !selected {
        Style::new().dim()
    } else {
        Role::Success.style().bold()
    };
    let label = ws.create_input.as_ref().map_or_else(
        || style.paint("+ new session"),
        |input| {
            format!(
                "{}{}",
                style.paint("+ new: "),
                widgets::block_caret(input.value(), input.cursor(), &style)
            )
        },
    );
    let mut rows = vec![widgets::pad_to_width(&format!("{cursor} {label}"), width)];
    if let Some(error) = &ws.create_error {
        rows.push(widgets::pad_to_width(
            &Role::Danger.style().paint(error),
            width,
        ));
    }
    rows
}

/// A real daemon-backed `SessionRecord` has a fixed two-line sidebar footprint.
/// The first line reserves the note glyph; the second projects only persisted
/// metadata, never a synthetic diff/GIF state or an executable shortcut.
#[coverage(off)]
fn session_menu_rows(
    width: usize,
    selected: bool,
    mode: Mode,
    session: &SessionRecord,
    git_diff: Option<&GitDiff>,
    columns: SidebarDiffColumns,
) -> Vec<String> {
    session_menu_rows_at(
        width,
        selected,
        mode,
        session,
        git_diff,
        columns,
        Utc::now(),
    )
}

/// 1 フレームでは同じ基準時刻を使うことで、複数 session が境界時刻に跨って別々の表現に
/// なることを避ける。
#[coverage(off)]
fn session_menu_rows_at(
    width: usize,
    selected: bool,
    mode: Mode,
    session: &SessionRecord,
    git_diff: Option<&GitDiff>,
    columns: SidebarDiffColumns,
    now: DateTime<Utc>,
) -> Vec<String> {
    let marker = if selected {
        match mode {
            Mode::Switch => Role::Danger.style().bold().paint("\u{f0907}"),
            Mode::Closeup => Role::Success.style().bold().paint("|"),
        }
    } else {
        " ".to_owned()
    };
    let label = widgets::clip_to_width(session.display_label(), width.saturating_sub(5));
    let label = match (selected, mode) {
        (true, _) => Role::Accent.style().bold().paint(&label),
        (false, Mode::Switch) => Style::new().dim().paint(&label),
        (false, Mode::Closeup) => Role::Accent.style().paint(&label),
    };
    let note = if session.notes.is_empty() {
        "·"
    } else {
        "✎"
    };
    let first = widgets::pad_to_width(
        &format!("{marker} {label}  {}", Style::new().dim().paint(note)),
        width,
    );
    let modified = widgets::relative_session_time(session.last_active_or_created(), now);
    let continuation = if selected {
        match mode {
            Mode::Switch => Role::Danger.style().bold().paint("|"),
            Mode::Closeup => Role::Success.style().bold().paint("|"),
        }
    } else {
        " ".to_owned()
    };
    let metadata = pr_summary(&session.prs).map_or_else(
        || format!("{continuation} {modified}"),
        |pr| format!("{continuation} {modified} · {pr}"),
    );
    let dim = mode == Mode::Switch && !selected;
    let metadata = sidebar_metadata(metadata, git_diff, columns, width, dim);
    vec![
        first,
        widgets::pad_to_width(
            &(if dim {
                Style::new().dim()
            } else {
                Style::new()
            })
            .paint(&metadata),
            width,
        ),
    ]
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
    let commits = match (columns.ahead > 0, columns.behind > 0) {
        (true, true) => format!(
            "↑{:>ahead$} ↓{:>behind$}",
            diff.ahead,
            diff.behind,
            ahead = columns.ahead,
            behind = columns.behind
        ),
        (true, false) => format!("↑{:>ahead$}", diff.ahead, ahead = columns.ahead),
        (false, true) => format!("↓{:>behind$}", diff.behind, behind = columns.behind),
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
        let accent = if dim {
            Role::Accent.style().dim()
        } else {
            Role::Accent.style()
        };
        format!("{} {lines}", accent.paint(&commits))
    }
}

/// v1 と同様に、作成中の session を実行前から同じ sidebar 内に予約する skeleton 行。
/// skeleton 自体は navigation target ではないため、cursor を持たない。名前と activity
/// glyph は同じ左から右へ流れる wave に乗せ、静的な点滅ではなく作成中であることを示す。
/// 実 session と同じ 2 行の高さを確保して、完了時の sidebar の揺れを防ぐ。
const PENDING_SESSION_WAVE_SPEED: usize = 4;

#[coverage(off)]
fn pending_session_row(width: usize, name: &str, frame: usize) -> String {
    let wave = widgets::Shimmer {
        speed: PENDING_SESSION_WAVE_SPEED,
        ..widgets::Shimmer::default()
    };
    let label = widgets::shimmer_text_with(name, frame, wave);
    let activity = widgets::shimmer_text_with("●", frame, wave);
    widgets::pad_to_width(&format!("  {activity} {label}"), width)
}

#[coverage(off)]
fn pending_session_rows(width: usize, name: &str, frame: usize) -> Vec<String> {
    vec![pending_session_row(width, name, frame), String::new()]
}

/// 左ペインの footer（キー操作ヒント、dim）。
#[coverage(off)]
fn left_footer(width: usize, ws: &Workspace) -> String {
    let hint = match ws.mode() {
        Mode::Switch => "[switch] ↑↓ select / Enter closeup",
        Mode::Closeup => "[closeup] Ctrl-O then: o switch / a/Ctrl-A actions / n/p tabs",
    };
    Style::new()
        .dim()
        .paint(&widgets::clip_to_width(hint, width))
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

/// 左ペイン（session menu）を `height` 行に組む。footer を最下行に
/// 固定し、残りを viewport として選択中の session / root 行を常に表示する。
#[coverage(off)]
fn left_pane(height: usize, width: usize, ws: &Workspace, skeleton_frame: usize) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    if height == 1 {
        return selectable_rows(width, ws, ws.selected)
            .into_iter()
            .take(1)
            .collect();
    }

    let body_capacity = height - 1;
    // Keep the menu usable first. The mascot block includes its always-reserved
    // blank row, so the viewport and footer cannot drift when speech adds rows.
    let metric_labels = mascot_metrics(ws.metrics.as_ref(), skeleton_frame);
    let mascot = widgets::mascot::sidebar_block_with_sidecar(
        width,
        skeleton_frame as u64,
        None,
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
    let start = workspace_viewport_start(ws.selected, ws, viewport_capacity);

    let mut rows = Vec::with_capacity(height);
    let now = Utc::now();
    let diff_columns = sidebar_diff_columns(ws.session_ids(), &ws.git_diffs);
    for index in start..ws.row_count() {
        let mut entry = if index == 0 {
            root_rows(width, ws)
        } else if index == ws.sessions().len() + 1 {
            create_session_rows(width, index == ws.selected, ws)
        } else {
            ws.sessions().get(index - 1).map_or_else(
                || root_rows(width, ws),
                |session| {
                    session_menu_rows_at(
                        width,
                        index == ws.selected,
                        ws.mode(),
                        session,
                        ws.git_diff(index - 1),
                        diff_columns,
                        now,
                    )
                },
            )
        };
        if index == ws.sessions().len() + 1
            && let Some(name) = ws.pending_session()
        {
            let mut pending = pending_session_rows(width, name, skeleton_frame);
            pending.append(&mut entry);
            entry = pending;
        }
        if rows.len() + entry.len() > viewport_capacity {
            break;
        }
        rows.extend(entry);
        if index == 0 && rows.len() < viewport_capacity {
            rows.push(sidebar_divider(width));
        }
    }
    rows.resize(content_capacity, String::new());
    if show_mascot {
        rows.extend(mascot.expect("shown mascot exists").rows().iter().cloned());
        rows.push(String::new());
    }
    rows.push(left_footer(width, ws));
    rows
}

// ── right pane: closeup ─────────────────────────────────────────────────────

/// closeup の header: フォーカス中 session の identity。
#[coverage(off)]
fn closeup_header(ws: &Workspace) -> String {
    format!(" {}", Role::Accent.style().bold().paint(ws.focused_label()))
}

/// tabmenu: pane reducer の stable selection を session 名の右の Chrome 風タブへ投影する。
#[coverage(off)]
fn tab_menu(width: usize, header: &str, ws: &Workspace, frame: usize) -> [String; 2] {
    let labels = ws
        .pane()
        .tabs()
        .iter()
        .map(pane_tab_label)
        .collect::<Vec<_>>();
    let tabs = ws
        .pane()
        .tabs()
        .iter()
        .zip(&labels)
        .map(|(tab, label)| widgets::session_tab::Tab {
            label,
            selected: pane_tab_selected(tab, ws.pane().selected()),
            pending_frame: matches!(tab, PaneTab::Pending(_)).then_some(frame as u64),
        })
        .collect::<Vec<_>>();
    widgets::session_tab::render_with_prefix(width, header, &tabs)
}

/// 右ペインの footer（キー操作ヒント、dim）。
#[coverage(off)]
fn right_footer(width: usize, ws: &Workspace) -> String {
    let hint = ws.terminal_feedback.as_deref().unwrap_or(match ws.mode() {
        Mode::Switch => "←→/hl tab / Enter/t closeup / : commands / p PR / q close / Ctrl-Q end",
        Mode::Closeup => "←→/hl tab / ↑↓/jk action / : commands / p PR / q close / Ctrl-Q end",
    });
    Style::new()
        .dim()
        .paint(&widgets::clip_to_width(hint, width))
}

/// 右ペイン（closeup）を `height` 行に組む: header・tabmenu・content、footer を最下行に固定。
#[coverage(off)]
fn right_pane(height: usize, width: usize, ws: &Workspace, frame: usize) -> Vec<String> {
    let header = closeup_header(ws);
    let mut rows = Vec::new();
    if ws.has_panes() {
        let chrome = tab_menu(width, &header, ws, frame);
        rows.extend(chrome);
        rows.push(String::new());
        if let Some(view) = &ws.terminal_view {
            // A focused live terminal renders daemon PTY output. Reserve the
            // footer and its breathing row, then clip each screen row to the
            // pane width.
            let content_cap = height.saturating_sub(rows.len() + 2);
            rows.extend(terminal_viewport_rows(
                view,
                ws.terminal_scroll,
                width,
                content_cap,
            ));
        } else if let Some(document) = ws.pane_document() {
            rows.extend(
                document
                    .iter()
                    .map(|line| widgets::pad_to_width(line, width)),
            );
        }
    } else {
        rows.push(widgets::pad_to_width(&header, width));
        rows.extend(widgets::session_tab::empty_pane(
            width,
            height.saturating_sub(3),
            "No tabs stirring yet. Enter starts one.",
        ));
    }
    with_footer_gap(rows, height, right_footer(width, ws))
}

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

/// 生の端末サイズに対する workspace 画面 1 フレーム分の行。全幅の header と罫線の下を、共通の
/// [`panes`] レイアウトで左（session menu）・右（closeup）の 2 ペインに割って組む。サイズ 0 は
/// 80×24 にフォールバックする。
#[must_use]
#[coverage(off)]
pub fn render(raw_height: usize, raw_width: usize, ws: &Workspace) -> Vec<String> {
    render_with_skeleton_frame(raw_height, raw_width, ws, 0)
}

/// [`render`] と同じ frame を描くが、pending session skeleton の shimmer 位相を指定する。
#[must_use]
#[coverage(off)]
pub fn render_with_skeleton_frame(
    raw_height: usize,
    raw_width: usize,
    ws: &Workspace,
    skeleton_frame: usize,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);

    let mut frame = Vec::with_capacity(height);
    frame.push(header_line(width, ws));
    frame.push(header_spacer(width));

    let body_height = height.saturating_sub(CHROME_ROWS);
    let split = panes::split(width, LEFT_WIDTH);
    let left = left_pane(body_height, split.left, ws, skeleton_frame);
    let right = dim_inactive_right_pane(
        ws.mode() == Mode::Switch,
        right_pane(body_height, split.right, ws, skeleton_frame),
    );
    frame.extend(panes::join(body_height, &left, &right, split));

    frame.truncate(height);
    frame
}

/// Converts a screen-cell pointer position into the retained terminal viewport
/// row and terminal column currently rendered in the right pane.
#[must_use]
#[coverage(off)]
pub fn terminal_point_at(
    raw_height: usize,
    raw_width: usize,
    ws: &Workspace,
    column: u16,
    row: u16,
) -> Option<TerminalPoint> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let right_left = right_pane_left(width);
    let body_row = usize::from(row).checked_sub(CHROME_ROWS)?;
    let column = usize::from(column).checked_sub(right_left)?;
    let view = ws.terminal_view.as_ref()?;
    let body_height = height.saturating_sub(CHROME_ROWS);
    let content_top = 3;
    let content_cap = body_height.saturating_sub(content_top + 2);
    let content_row = body_row.checked_sub(content_top)?;
    if content_row >= content_cap {
        return None;
    }
    let start = view
        .len()
        .saturating_sub(content_cap.saturating_add(ws.terminal_scroll));
    Some(TerminalPoint {
        row: start + content_row,
        column,
    })
}

/// Returns the auto-scroll direction for a pointer held at either terminal
/// viewport edge: `true` is toward older output and `false` toward live output.
#[must_use]
#[coverage(off)]
pub fn terminal_auto_scroll_direction_at(
    raw_height: usize,
    raw_width: usize,
    ws: &Workspace,
    column: u16,
    row: u16,
) -> Option<bool> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let split = panes::split(width, LEFT_WIDTH);
    if usize::from(column) < split.left.saturating_add(1) || ws.terminal_view.is_none() {
        return None;
    }
    let body_row = usize::from(row).checked_sub(CHROME_ROWS)?;
    let content_top = 3;
    let content_cap = height.saturating_sub(CHROME_ROWS + content_top + 2);
    // The first and last output rows are ordinary selectable cells. Only a
    // pointer *outside* the viewport starts edge auto-scroll; treating the
    // first row as an edge made that row impossible to select reliably.
    (body_row < content_top)
        .then_some(true)
        .or_else(|| (body_row >= content_top.saturating_add(content_cap)).then_some(false))
}

/// The selectable row currently visible at the requested auto-scroll edge.
#[must_use]
#[coverage(off)]
pub fn terminal_edge_point(
    raw_height: usize,
    raw_width: usize,
    ws: &Workspace,
    older: bool,
) -> Option<TerminalPoint> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let split = panes::split(width, LEFT_WIDTH);
    let view = ws.terminal_view.as_ref()?;
    let content_cap = height.saturating_sub(CHROME_ROWS + 3 + 2);
    let start = view
        .len()
        .saturating_sub(content_cap.saturating_add(ws.terminal_scroll));
    Some(TerminalPoint {
        row: if older {
            start
        } else {
            start.saturating_add(content_cap.saturating_sub(1))
        },
        column: split.right.saturating_sub(1),
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
    if let Some(overlay) = &home.decision_overlay {
        decision_modal::render_over(height, width, &frame, overlay, &home.decisions)
    } else if home.closeup_action_visible {
        crate::presentation::views::closeup_modal::render_over(
            height,
            width,
            &frame,
            &CloseupModal::new(home.active_label()),
        )
    } else {
        frame
    }
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

/// Number of sidebar body lines `home_row_lines_at` renders for `row`, excluding
/// the divider that `home_left_pane` inserts after the Root row. Root and the
/// action row are single-line; a session identity row carries a metadata row.
/// `home_row_height` weights Root as 2 for scroll math (its line plus divider);
/// this returns the drawn line count so pointer hit-tests match the frame.
fn home_row_content_lines(row: Selection) -> usize {
    match row {
        Selection::Target(Target::Session(_)) => 2,
        Selection::Target(Target::Root(_)) | Selection::NewSession => 1,
    }
}

/// Rows the mascot sidecar reserves at the foot of the sidebar for a given body
/// capacity. This mirrors the reservation `home_left_pane` computes inline while
/// rendering; `HomeProjection::row_at` calls it so the hit-test agrees with the
/// frame on how much room the session list actually has.
fn home_mascot_rows(home: &HomeProjection, width: usize, body_capacity: usize) -> usize {
    let metric_labels = home
        .metrics
        .as_ref()
        .map(|metrics| mascot_metrics(Some(metrics), 0))
        .unwrap_or_default();
    widgets::mascot::sidebar_block_with_sidecar(
        width,
        home.mascot_tick,
        home.mascot_speech.as_ref(),
        &metric_labels,
    )
    .as_ref()
    .filter(|block| body_capacity >= block.reserved_rows() + 2)
    .map_or(0, widgets::mascot::MascotBlock::reserved_rows)
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
        CHROME_ROWS, GitDiff, HomeProjection, LEFT_WIDTH, Mode, ProjectedSession,
        TerminalViewProjection, Workspace, render, render_home, render_with_skeleton_frame,
        sidebar_row_at, terminal_auto_scroll_direction_at, terminal_point_at, with_footer_gap,
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

    const MASCOT_INDENT: usize = 1;
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

    #[test]
    fn terminal_scroll_is_bounded_by_retained_history_and_returns_to_live_bottom() {
        let mut ws = workspace();
        ws.set_terminal_view(Some(vec![
            "old".to_string(),
            "middle".to_string(),
            "live".to_string(),
        ]));
        ws.terminal_scroll_up();
        ws.terminal_scroll_up();
        ws.terminal_scroll_up();
        assert_eq!(ws.terminal_scroll, 2);
        ws.terminal_scroll_down();
        assert_eq!(ws.terminal_scroll, 1);
        ws.set_terminal_view(Some(vec!["live".to_string()]));
        assert_eq!(
            ws.terminal_scroll, 0,
            "a shorter replay normalizes stale scroll"
        );
    }

    #[test]
    fn terminal_auto_scroll_keeps_the_first_output_row_selectable() {
        let mut ws = workspace();
        ws.set_terminal_view(Some(vec!["first".to_string(), "second".to_string()]));

        assert_eq!(
            terminal_auto_scroll_direction_at(24, 80, &ws, 37, 5),
            None,
            "the first visible output row is a selectable cell, not an edge"
        );
        assert_eq!(
            terminal_auto_scroll_direction_at(24, 80, &ws, 37, 4),
            Some(true)
        );
    }

    #[test]
    fn terminal_point_tracks_the_visible_scrollback_window() {
        let mut ws = workspace();
        ws.set_terminal_view(Some((0..20).map(|row| format!("row {row}")).collect()));

        assert_eq!(
            terminal_point_at(24, 80, &ws, 37, 5),
            Some(TerminalPoint { row: 3, column: 0 })
        );
        assert_eq!(terminal_point_at(24, 80, &ws, 37, 4), None);

        ws.terminal_scroll_up();
        ws.terminal_scroll_up();
        assert_eq!(
            terminal_point_at(24, 80, &ws, 37, 5),
            Some(TerminalPoint { row: 1, column: 0 })
        );
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
            last_modified: now(),
            has_notes: false,
            pr_summary: None,
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

    #[test]
    fn row_at_maps_pointer_clicks_to_sidebar_selections() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let snapshot = vec![projected_session(session, "alpha", "/work/alpha")];
        let home = HomeProjection::from_state(&state, "work", "/work", &snapshot);

        // Content begins after the two chrome rows: the Root identity line (row 2),
        // the divider (row 3), the session's two lines (rows 4-5), then the action
        // row. A click below every rendered row resolves to nothing.
        assert_eq!(
            home.row_at(30, 100, 5, 2),
            Some(Selection::Target(Target::Root(workspace)))
        );
        assert_eq!(home.row_at(30, 100, 5, 3), None);
        assert_eq!(
            home.row_at(30, 100, 5, 4),
            Some(Selection::Target(Target::Session(session)))
        );
        assert_eq!(
            home.row_at(30, 100, 5, 5),
            Some(Selection::Target(Target::Session(session)))
        );
        assert_eq!(home.row_at(30, 100, 5, 6), Some(Selection::NewSession));
        assert_eq!(home.row_at(30, 100, 5, 8), None);
    }

    #[test]
    fn row_at_rejects_clicks_outside_the_sidebar_body() {
        let workspace = WorkspaceId::new();
        let state = AppState::home(workspace, Vec::new());
        let home = HomeProjection::from_state(&state, "work", "/work", &[]);
        assert_eq!(home.row_at(30, 100, 90, 4), None); // right pane column
        assert_eq!(home.row_at(30, 100, 5, 0), None); // header row
        assert_eq!(home.row_at(30, 100, 5, 1), None); // spacer row
        assert_eq!(home.row_at(2, 100, 5, 2), None); // height at/under the chrome
        assert_eq!(home.row_at(8, 100, 5, 7), None); // click past content capacity
    }

    #[test]
    fn row_at_handles_single_body_line_and_overflowing_rows() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let snapshot = vec![projected_session(session, "alpha", "/work/alpha")];
        let home = HomeProjection::from_state(&state, "work", "/work", &snapshot);
        // body_height == 1: only the first row is addressable.
        assert_eq!(
            home.row_at(3, 100, 5, 2),
            Some(Selection::Target(Target::Root(workspace)))
        );
        assert_eq!(home.row_at(3, 100, 5, 5), None);
        // At height 6 the content capacity is 3, so the session's two lines
        // overflow after the Root row and divider and cannot be hit.
        assert_eq!(home.row_at(6, 100, 5, 4), None);
    }

    #[test]
    fn row_at_shares_the_mascot_reservation_with_the_metrics_sidecar() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let snapshot = vec![projected_session(session, "alpha", "/work/alpha")];
        let metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
        };
        let home = HomeProjection::from_state(&state, "work", "/work", &snapshot)
            .with_metrics(Some(metrics));
        // The mascot sidecar (fed by daemon metrics) reserves rows at the foot of
        // the sidebar; the session list above it still hit-tests to the same rows.
        assert_eq!(
            home.row_at(30, 100, 5, 2),
            Some(Selection::Target(Target::Root(workspace)))
        );
        assert_eq!(
            home.row_at(30, 100, 5, 4),
            Some(Selection::Target(Target::Session(session)))
        );
    }

    #[test]
    fn pointer_select_row_moves_the_cursor_through_the_reducer() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        let snapshot = vec![projected_session(session, "alpha", "/work/alpha")];
        let home = HomeProjection::from_state(&state, "work", "/work", &snapshot);

        let hit = home
            .row_at(30, 100, 5, 4)
            .expect("session row is addressable");
        assert_eq!(hit, Selection::Target(Target::Session(session)));
        let effects = update(&mut state, AppEvent::Key(AppKey::SelectRow(hit)));
        assert!(effects.is_empty());
        assert_eq!(
            state.selected(),
            Selection::Target(Target::Session(session))
        );

        // A stale click naming a session that no longer exists leaves the cursor
        // where it is rather than pointing at a vanished row.
        let ghost = Selection::Target(Target::Session(SessionId::new()));
        let _ = update(&mut state, AppEvent::Key(AppKey::SelectRow(ghost)));
        assert_eq!(
            state.selected(),
            Selection::Target(Target::Session(session))
        );
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
    fn home_metrics_sidecar_matches_the_legacy_daemon_metrics_row() {
        let metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
        };

        // Legacy path: the daemon observation flows through `set_metrics`.
        let mut ws = workspace();
        ws.set_metrics(Some(metrics.clone()));
        let legacy = render_with_skeleton_frame(30, 100, &ws, 0);

        // Controller path: the same observation flows through `with_metrics`.
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let home = HomeProjection::from_state(&state, "actual", "/tmp/actual", &[])
            .with_metrics(Some(metrics));
        let controller = render_home(30, 100, &home);

        let metric_row = |frame: &[String]| {
            frame
                .iter()
                .find(|line| line.contains('\u{f2db}'))
                .expect("daemon metric row beside usagi")
                .clone()
        };
        let legacy_row = metric_row(&legacy);
        let controller_row = metric_row(&controller);

        // Parity covers both the glyphs and the load-aware colour styling, so the
        // raw ANSI rows must be identical, not only their stripped text.
        assert_eq!(strip(&controller_row), strip(&legacy_row));
        assert_eq!(controller_row, legacy_row);
        assert!(strip(&controller_row).contains("\u{f2db} 1%    \u{f233} 45MB"));
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
    fn workspace_is_built_from_domain_records() {
        let ws = workspace();
        assert_eq!(ws.name(), "actual");
        assert_eq!(ws.path(), PathBuf::from("/tmp/actual"));
        assert_eq!(ws.sessions().len(), 2);
        assert_eq!(ws.sessions()[0].display_label(), "UI work");
        assert!(!ws.has_panes());
        assert_eq!(ws.mode(), Mode::Switch);
        assert_eq!(ws.selected(), 0);
        assert_eq!(ws.active_tab(), 0);
        assert!(ws.root_selected());
        assert!(format!("{:?}", ws.clone()).contains("actual"));
        assert!(format!("{:?}", ws.pane()).contains("PaneState"));
    }

    #[test]
    fn daemon_snapshot_replaces_sidebar_rows_without_persisting_legacy_state() {
        let mut ws = workspace();
        ws.select_next();

        ws.replace_sessions(vec![session("fresh", None, SessionOrigin::Unknown)]);

        assert_eq!(ws.sessions().len(), 1);
        assert_eq!(ws.sessions()[0].name, "fresh");
        assert!(ws.root_selected());
    }

    #[test]
    fn sidebar_hit_test_resolves_visible_session_rows_only() {
        let ws = workspace();
        // Two chrome rows, the two-line root, then its divider precede the
        // two-line session rows in the sidebar.
        assert_eq!(sidebar_row_at(24, 100, &ws, 0, 0, 5), Some(1));
        assert_eq!(sidebar_row_at(24, 100, &ws, 0, 0, 7), Some(2));
        assert_eq!(sidebar_row_at(24, 100, &ws, 0, 0, 4), None);
        assert_eq!(sidebar_row_at(24, 100, &ws, 0, 36, 5), None);
    }

    #[test]
    fn created_session_snapshot_is_selectable_and_has_an_empty_pane_state() {
        let mut ws = Workspace::new(
            WorkspaceRecord::new("empty", "/tmp/empty"),
            WorkspaceState::new(),
        );

        // This is the snapshot delivered when the create skeleton completes.
        ws.replace_sessions(vec![session("fresh", None, SessionOrigin::Unknown)]);

        ws.select_next();
        assert_eq!(
            ws.selected_session().map(|session| session.name.as_str()),
            Some("fresh")
        );
        assert!(
            ws.pane().tabs().is_empty(),
            "new rows have a pane projection"
        );

        ws.enter_closeup();
        assert!(
            joined(&ws).contains("fresh"),
            "Closeup renders the new session"
        );
    }

    #[test]
    fn snapshot_removal_discards_the_removed_session_pane_state() {
        let mut ws = workspace();
        assert!(ws.panes.contains_key("tui"));

        ws.replace_sessions(vec![session("daemon", None, SessionOrigin::Unknown)]);

        assert!(!ws.panes.contains_key("tui"));
        assert!(ws.panes.contains_key("daemon"));
    }

    #[test]
    fn select_cycles_from_the_root_through_sessions() {
        let mut ws = workspace();
        ws.select_next();
        assert_eq!(ws.selected(), 1);
        ws.select_next();
        assert_eq!(ws.selected(), 2);
        ws.select_next();
        assert!(ws.new_session_selected());
        let rendered = render(30, 100, &ws).join("\n");
        assert!(rendered.contains("\u{1b}[1;32m+ new session\u{1b}[0m"));
        assert!(!rendered.contains("+ new session  action"));
        ws.select_next();
        assert!(ws.root_selected());
        ws.select_prev();
        assert!(ws.new_session_selected());
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
        assert!(text.contains("USAGI > empty"));
        assert!(!text.contains("/tmp/empty"));
        assert!(text.contains("─"));
        assert!(text.contains("+ new session"));
    }

    #[test]
    fn pane_tab_navigation_wraps_and_close_returns_to_empty_state() {
        let mut ws = workspace();
        ws.open_pane(PaneKind::Terminal);
        ws.open_pane(PaneKind::Agent);
        ws.tab_prev();
        assert!(matches!(ws.pane().selected(), PaneSelection::Tab(_)));
        ws.tab_next();
        assert!(joined(&ws).contains("Terminal"));
        assert!(joined(&ws).contains("Agent"));
        ws.close_pane();
        ws.close_pane();
        assert!(!ws.has_panes());
    }

    #[test]
    fn pane_tabs_are_scoped_to_the_selected_session() {
        let mut ws = workspace();

        ws.select_next();
        ws.open_pane(PaneKind::Agent);
        assert_eq!(ws.pane().tabs().len(), 1);

        ws.select_next();
        assert!(ws.pane().tabs().is_empty());
        ws.open_pane(PaneKind::Terminal);
        assert_eq!(ws.pane().tabs().len(), 1);

        ws.select_prev();
        assert!(
            matches!(ws.pane().tabs(), [PaneTab::Pending(pending)] if pending.kind == PaneKind::Agent)
        );
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
        assert_eq!(ws.active_tab(), 0);
        ws.apply_home_mode(HomeMode::Switch);
        assert_eq!(ws.mode(), Mode::Switch);
    }

    #[test]
    fn focused_label_and_pull_requests_follow_the_selected_session() {
        let mut ws = workspace();
        ws.state.sessions[0]
            .prs
            .push(PrLink::new(42, "https://example.com/pull/42"));

        assert_eq!(ws.focused_label(), "main");
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
        let switch_frame = render(30, 100, &ws);
        let switch_header = &switch_frame[0];
        assert!(switch_header.contains("\u{1b}[1;36m\u{f0ec} switch\u{1b}[0m"));
        assert!(switch_header.contains("\u{1b}[2m\u{f00e} closeup\u{1b}[0m"));
        assert!(
            strip(switch_header)
                .trim_end()
                .ends_with("\u{f0ec} switch  \u{f00e} closeup")
        );
        assert!(strip(&switch_frame[1]).trim().is_empty());

        ws.enter_closeup();
        let closeup_header = &render(30, 100, &ws)[0];
        assert!(closeup_header.contains("\u{1b}[2m\u{f0ec} switch\u{1b}[0m"));
        assert!(closeup_header.contains("\u{1b}[1;36m\u{f00e} closeup\u{1b}[0m"));
        assert!(
            strip(closeup_header)
                .trim_end()
                .ends_with("\u{f0ec} switch  \u{f00e} closeup")
        );
    }

    #[test]
    fn render_uses_mode_specific_footers_and_renders_chrome_tabs_from_pane_state() {
        let mut ws = workspace();
        let switch = joined(&ws);
        assert!(switch.contains("[switch] ↑↓ select"));
        assert!(switch.contains("←→/hl tab"));
        assert!(switch.contains("Enter/t closeup"));
        assert!(switch.contains("p PR"));
        assert!(switch.contains("No tabs stirring yet. Enter starts one."));

        ws.open_pane(PaneKind::Terminal);
        ws.enter_closeup();
        let closeup_frame = render(30, 100, &ws);
        let closeup = closeup_frame
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(closeup.contains("[closeup] Ctrl-O then"));
        assert!(closeup.contains("←→/hl tab"));
        assert!(!closeup.contains("Esc switch"));
        assert!(closeup.contains("↑↓/jk action"));
        assert!(closeup.contains("Terminal"));
        assert!(
            !closeup.contains('▔'),
            "a pending tab remains listed but does not take focus before completion"
        );
    }

    #[test]
    fn runtime_workspace_renderer_dims_the_right_pane_only_in_switch() {
        let mut ws = workspace();
        ws.open_pane(PaneKind::Terminal);

        let switch_frame = render(30, 100, &ws);
        let switch_right = switch_frame[CHROME_ROWS]
            .split_once('│')
            .expect("pane divider")
            .1;
        assert!(switch_right.contains("\u{1b}[2;36mmain"));
        assert!(!switch_right.contains("\u{1b}[1;36mmain"));

        ws.enter_closeup();
        let closeup_frame = render(30, 100, &ws);
        let closeup_right = closeup_frame[CHROME_ROWS]
            .split_once('│')
            .expect("pane divider")
            .1;
        assert!(closeup_right.contains("\u{1b}[1;36mmain"));
        assert!(!closeup_right.starts_with("\u{1b}[2m"));
    }

    #[test]
    fn render_shows_real_workspace_and_session_records() {
        let text = joined(&workspace());
        assert!(text.contains("USAGI"));
        assert!(text.contains("actual"));
        assert!(text.contains("USAGI > actual"));
        assert!(!text.contains("Sessions"));
        assert!(text.contains("UI work"));
        assert!(text.contains("daemon"));
        assert!(!text.contains("UTC"));
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
        assert!(!text.contains("/tmp/actual"));
        assert!(text.contains("main"));
        assert!(!text.contains("Esc back"));
        assert!(text.contains('│'));
    }

    #[test]
    fn session_rows_project_legacy_time_note_and_visible_prs_without_false_affordances() {
        let id = SessionId::new();
        let mut record = session("日本語-session", None, SessionOrigin::Unknown);
        record.last_active = Some(
            DateTime::parse_from_rfc3339("2026-06-26T13:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        record.notes.note = Some("keep this visible".to_owned());
        record
            .prs
            .push(PrLink::new(42, "https://example.test/pull/42"));
        let mut dismissed = PrLink::new(99, "https://example.test/pull/99");
        dismissed.state = PrState::Dismissed;
        record.prs.push(dismissed);

        let projection = ProjectedSession::from_record(id, &record);
        assert_eq!(projection.id, id);
        assert_eq!(projection.last_modified, record.last_active.unwrap());
        assert!(projection.has_notes);
        assert_eq!(projection.pr_summary.as_deref(), Some("PR #42"));

        let base = DateTime::parse_from_rfc3339("2026-06-26T13:42:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let rows = super::session_menu_rows_at(
            40,
            true,
            Mode::Switch,
            &record,
            None,
            super::SidebarDiffColumns::default(),
            base,
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("\u{1b}[1;31m\u{f0907}\u{1b}[0m"));
        assert!(rows[1].contains("\u{1b}[1;31m|\u{1b}[0m"));
        assert!(strip(&rows[0]).contains("✎"));
        assert!(strip(&rows[1]).contains("12m ago"));
        assert!(strip(&rows[1]).contains("PR #42"));
        assert!(!strip(&rows[1]).contains("diff"));
        assert!(rows.iter().all(|row| display_width(row) == 40));
        let narrow = super::session_menu_rows_at(
            18,
            true,
            Mode::Switch,
            &record,
            None,
            super::SidebarDiffColumns::default(),
            base,
        );
        assert!(strip(&narrow[0]).contains("✎"));
        assert!(narrow.iter().all(|row| display_width(row) == 18));

        let closeup = super::session_menu_rows_at(
            40,
            true,
            Mode::Closeup,
            &record,
            None,
            super::SidebarDiffColumns::default(),
            base,
        );
        assert!(closeup[0].contains("\u{1b}[1;32m|\u{1b}[0m"));
        assert!(closeup[1].contains("\u{1b}[1;32m|\u{1b}[0m"));
    }

    #[test]
    fn session_row_shows_completed_git_state_and_changed_lines() {
        let record = session("topic", None, SessionOrigin::Unknown);
        let base = DateTime::parse_from_rfc3339("2026-06-26T13:42:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let diff = GitDiff {
            base: "origin/main".to_owned(),
            ahead: 2,
            behind: 1,
            added: 12,
            removed: 4,
        };
        let columns = super::SidebarDiffColumns {
            ahead: 1,
            behind: 1,
            added: 2,
            removed: 1,
        };
        let rows = super::session_menu_rows_at(
            60,
            false,
            Mode::Switch,
            &record,
            Some(&diff),
            columns,
            base,
        );
        let plain = strip(&rows[1]);
        assert!(plain.contains("↑2 ↓1"));
        assert!(plain.contains("+ 12 - 4"));
        assert!(!plain.contains("origin/main"));
        assert!(rows[1].contains("\u{1b}[2;32m+ 12\u{1b}[0m"));
        assert!(rows[1].contains("\u{1b}[2;31m- 4\u{1b}[0m"));
        assert!(rows.iter().all(|row| display_width(row) == 60));

        let selected = super::session_menu_rows_at(
            60,
            true,
            Mode::Switch,
            &record,
            Some(&diff),
            columns,
            base,
        );
        assert!(selected[1].contains("\u{1b}[32m+ 12\u{1b}[0m"));
        assert!(selected[1].contains("\u{1b}[31m- 4\u{1b}[0m"));
        assert!(!selected[1].contains("\u{1b}[2;32m+ 12\u{1b}[0m"));
        assert!(!selected[1].contains("\u{1b}[2;31m- 4\u{1b}[0m"));

        let narrow = super::session_menu_rows_at(
            18,
            false,
            Mode::Switch,
            &record,
            Some(&diff),
            columns,
            base,
        );
        assert!(narrow.iter().all(|row| display_width(row) == 18));
    }

    #[test]
    fn session_git_summary_columns_are_shared_by_every_session() {
        let base = DateTime::parse_from_rfc3339("2026-06-26T13:42:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let columns = super::SidebarDiffColumns {
            ahead: 2,
            behind: 1,
            added: 4,
            removed: 3,
        };
        let first = GitDiff {
            base: "origin/main".to_owned(),
            ahead: 1,
            behind: 2,
            added: 12,
            removed: 4,
        };
        let second = GitDiff {
            base: "origin/main".to_owned(),
            ahead: 10,
            behind: 1,
            added: 1234,
            removed: 567,
        };
        let first = super::session_menu_rows_at(
            70,
            false,
            Mode::Switch,
            &session("one", None, SessionOrigin::Unknown),
            Some(&first),
            columns,
            base,
        );
        let second = super::session_menu_rows_at(
            70,
            false,
            Mode::Switch,
            &session("two", None, SessionOrigin::Unknown),
            Some(&second),
            columns,
            base,
        );
        assert_eq!(strip(&first[1]).find('+'), strip(&second[1]).find('+'));
        assert_eq!(strip(&first[1]).find('-'), strip(&second[1]).find('-'));
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

    #[test]
    fn render_joins_git_diffs_to_the_matching_stable_session_id() {
        let mut ws = workspace();
        let daemon = ws.session_ids()[1];
        ws.set_git_diffs(BTreeMap::from([(
            daemon,
            GitDiff {
                base: "origin/main".to_owned(),
                ahead: 3,
                behind: 2,
                added: 8,
                removed: 1,
            },
        )]));

        let rendered = render(30, 100, &ws)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("daemon"));
        assert!(rendered.contains("↑3 ↓2 + 8 - 1"));
        assert!(!rendered.contains("origin/main"));
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
    fn pending_tab_is_listed_with_a_wave_and_focuses_only_when_completion_is_uninterrupted() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let mut ws = Workspace::with_runtime_ids(
            WorkspaceRecord::new("actual", "/tmp/actual"),
            WorkspaceState {
                sessions: vec![session("tui", None, SessionOrigin::Human)],
                root_notes: Scratchpad::default(),
                updated_at: now(),
            },
            workspace_id,
            vec![session_id],
        );
        ws.select_next();
        ws.enter_closeup();
        let operation = ws.open_pane(PaneKind::Terminal);

        let early = render_with_skeleton_frame(30, 100, &ws, 0).join("\n");
        let later = render_with_skeleton_frame(30, 100, &ws, 12).join("\n");
        assert!(strip(&early).contains("Terminal"));
        assert_ne!(early, later, "the pending tab uses the shared loading wave");

        let terminal = terminal_ref(workspace_id, session_id);
        ws.complete_pane(operation, terminal.clone());
        assert_eq!(
            ws.pane().selected(),
            &PaneSelection::Tab(TabSelection::Live(terminal))
        );
    }

    #[test]
    fn later_interaction_cancels_pending_tab_completion_focus() {
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let mut ws = Workspace::with_runtime_ids(
            WorkspaceRecord::new("actual", "/tmp/actual"),
            WorkspaceState {
                sessions: vec![session("tui", None, SessionOrigin::Human)],
                root_notes: Scratchpad::default(),
                updated_at: now(),
            },
            workspace_id,
            vec![session_id],
        );
        ws.select_next();
        ws.enter_closeup();
        let operation = ws.open_pane(PaneKind::Agent);
        let selected_before_completion = ws.pane().selected().clone();
        ws.record_interaction();

        ws.complete_pane(operation, terminal_ref(workspace_id, session_id));
        assert_eq!(ws.pane().selected(), &selected_before_completion);
    }

    #[test]
    fn home_sidebar_git_columns_match_the_legacy_diff_row() {
        let diff = GitDiff {
            base: "origin/main".to_owned(),
            ahead: 3,
            behind: 2,
            added: 8,
            removed: 1,
        };

        // Legacy path: the observation flows through `set_git_diffs`.
        let mut ws = workspace();
        let daemon_id = ws.session_ids()[1];
        ws.set_git_diffs(BTreeMap::from([(daemon_id, diff.clone())]));
        let legacy = render(30, 100, &ws);

        // Controller path: the same observation flows through `with_git_diffs`.
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

        let diff_row = |frame: &[String]| {
            frame
                .iter()
                .map(|line| strip(line))
                .find(|line| line.contains("↑3 ↓2"))
                .expect("git diff row")
        };
        // The ported diff column is byte-identical after stripping ANSI: same
        // summary text and the same right-aligned column positions.
        assert_eq!(diff_row(&controller), diff_row(&legacy));
        assert!(diff_row(&controller).contains("↑3 ↓2 + 8 - 1"));
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

        // Legacy path: a focused live terminal renders daemon PTY output and its
        // feedback flows through `set_terminal_feedback`.
        let mut ws = workspace();
        ws.enter_closeup();
        let operation = ws.open_pane(PaneKind::Terminal);
        ws.complete_pane(operation, terminal_ref(workspace_id, session));
        ws.set_terminal_view(Some(view_rows.clone()));
        ws.set_terminal_feedback(Some("copied 3 lines".to_owned()));
        let legacy = render(30, 100, &ws);

        // Controller path: the same rows and feedback flow through
        // `with_terminal_view`.
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
        let legacy_right = right_pane(&legacy);
        // The live viewport rows and the terminal feedback both appear, matching
        // the information the legacy pane draws.
        assert!(controller_right.iter().any(|line| line == "live row"));
        assert!(controller_right.iter().any(|line| line == "old row"));
        assert!(legacy_right.iter().any(|line| line == "live row"));
        // The terminal feedback surfaces in the right-pane footer on both paths.
        let footer_feedback = |frame: &[String]| {
            frame
                .iter()
                .any(|line| strip(line).contains("copied 3 lines"))
        };
        assert!(footer_feedback(&controller));
        assert!(footer_feedback(&legacy));
        // The shared viewport window keeps the newest row anchored to the bottom
        // of the content area on both paths.
        let bottom_output = |right: &[String]| {
            right
                .iter()
                .rfind(|line| line.ends_with(" row"))
                .cloned()
                .expect("a rendered output row")
        };
        assert_eq!(
            bottom_output(&controller_right),
            bottom_output(&legacy_right)
        );
    }

    #[test]
    fn home_terminal_scroll_offset_matches_the_legacy_window() {
        let workspace_id = WorkspaceId::new();
        let session = SessionId::new();
        let rows = (0..20).map(|row| format!("row {row}")).collect::<Vec<_>>();

        let mut ws = workspace();
        ws.enter_closeup();
        let operation = ws.open_pane(PaneKind::Terminal);
        ws.complete_pane(operation, terminal_ref(workspace_id, session));
        ws.set_terminal_view(Some(rows.clone()));
        ws.terminal_scroll_up();
        ws.terminal_scroll_up();
        let legacy = render(24, 80, &ws);

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
        assert_eq!(output_rows(&controller), output_rows(&legacy));
        assert!(!output_rows(&controller).is_empty());
        // A two-row scrollback offset keeps the live tail hidden on both paths.
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
    fn workspace_renderer_uses_v1_colours_for_switch_and_closeup_rows() {
        let mut ws = workspace();
        ws.select_next();

        let switch = render(30, 100, &ws).join("\n");
        assert!(switch.contains("\u{1b}[1;36mUI work\u{1b}[0m"));
        assert!(switch.contains("\u{1b}[2mdaemon\u{1b}[0m"));
        assert!(switch.contains("\u{1b}[2m+ new session\u{1b}[0m"));

        ws.enter_closeup();
        let closeup = render(30, 100, &ws).join("\n");
        assert!(closeup.contains("\u{1b}[1;36mUI work\u{1b}[0m"));
        assert!(closeup.contains("\u{1b}[36mdaemon\u{1b}[0m"));
        assert!(closeup.contains("\u{1b}[1;32m+ new session\u{1b}[0m"));
    }

    #[test]
    fn pending_session_is_rendered_as_a_non_selectable_shimmer_skeleton() {
        let mut ws = workspace();
        ws.begin_pending_session("feature-x".to_owned());

        let frame = render_with_skeleton_frame(30, 100, &ws, 4);
        let text = frame
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("feature-x"));
        assert!(text.contains('●'));
        let skeleton = text.find("feature-x").unwrap();
        let create = text.find("+ new session").unwrap();
        assert!(
            skeleton < create,
            "skeleton is immediately above new session"
        );
        assert_eq!(
            text.matches("> feature-x").count(),
            0,
            "a skeleton must not become a navigation target"
        );

        ws.clear_pending_session();
        let cleared = render(30, 100, &ws)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!cleared.contains("feature-x"));
    }

    #[test]
    fn pending_session_skeleton_uses_the_shared_shimmer_wave() {
        let first = super::pending_session_row(100, "feature-x", 0);
        let held = super::pending_session_row(100, "feature-x", 3);
        let next = super::pending_session_row(100, "feature-x", 4);

        assert_eq!(first, held, "the pending session wave advances slowly");
        assert_ne!(first, next, "the pending session name sweeps");
    }

    #[test]
    fn pending_session_skeleton_reserves_the_same_two_rows_as_a_session() {
        let rows = super::pending_session_rows(30, "feature-x", 0);

        assert_eq!(rows.len(), 2);
        assert!(strip(&rows[0]).contains("feature-x"));
        assert!(rows[1].is_empty());
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

    #[test]
    fn render_places_the_v1_style_usagi_above_the_left_footer() {
        let frame = render(30, 100, &workspace());
        let left_width = LEFT_WIDTH;
        let left_rows = frame[CHROME_ROWS..]
            .iter()
            .map(|line| strip(line).chars().take(left_width).collect::<String>())
            .collect::<Vec<_>>();

        let ears = left_rows
            .iter()
            .position(|line| line.contains("(\\(\\"))
            .expect("sidebar ears");
        assert!(left_rows[ears + 1].contains("(o.o)?"));
        assert!(left_rows[ears + 2].contains("o(_(\")(\")"));
        assert!(left_rows[ears + 3].trim().is_empty(), "reserved blank row");
        assert!(left_rows[ears + 4].contains("[switch]"));
        assert_eq!(left_rows[ears].find('('), Some(MASCOT_INDENT + 1));
        assert_eq!(left_rows[ears + 1].find('('), Some(MASCOT_INDENT + 1));
        assert_eq!(left_rows[ears + 2].find('o'), Some(MASCOT_INDENT));
    }

    #[test]
    fn render_places_daemon_metrics_to_the_right_of_usagi() {
        let mut ws = workspace();
        let metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 1,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
        };
        assert!(
            super::mascot_metrics(Some(&metrics), 0)
                .concat()
                .contains("\u{1b}[2;37m\u{f2db}")
        );
        ws.set_metrics(Some(metrics));
        let frame = render(30, 100, &ws);
        let left_rows = frame[CHROME_ROWS..]
            .iter()
            .map(|line| strip(line).chars().take(LEFT_WIDTH).collect::<String>())
            .collect::<Vec<_>>();
        let metrics = left_rows
            .iter()
            .position(|line| line.contains('\u{f2db}'))
            .expect("CPU beside usagi");
        assert!(left_rows[metrics].contains("\u{f2db} 1%    \u{f233} 45MB"));
    }

    #[test]
    fn render_prioritizes_the_session_menu_over_the_usagi_when_short() {
        let frame = render(6, 100, &workspace());
        let text = frame
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("\u{f0907} main"));
        assert!(!text.contains("(o.o)?"));
    }

    #[test]
    fn render_places_the_selected_root_before_every_session() {
        let text = joined(&workspace());
        let root = text.find("\u{f0907} main").expect("selected main row");
        let first = text.find("UI work").expect("first session row");
        let second = text.find("daemon").expect("second session row");
        assert!(root < first);
        assert!(first < second);
        assert!(text[root..first].contains('─'));
    }

    #[test]
    fn render_reflects_selected_session_and_root() {
        let mut ws = workspace();
        let root_text = joined(&ws);
        assert!(root_text.contains("No tabs stirring yet. Enter starts one."));
        assert!(!root_text.contains("/tmp/actual"));

        ws.select_next();
        let session_text = joined(&ws);
        assert!(session_text.contains("UI work"));
        assert!(session_text.contains("No tabs stirring yet. Enter starts one."));

        ws.open_pane(PaneKind::Terminal);
        let frame = render(30, 100, &ws);
        let right_header = strip(&frame[CHROME_ROWS]);
        let name = right_header.find("UI work").expect("session name");
        let tab = right_header.find("Terminal").expect("terminal tab");
        assert!(name < tab);
        assert!(!frame.iter().any(|line| strip(line).contains("/tmp/actual")));
        ws.close_pane();

        ws.select_next();
        let second_session_text = joined(&ws);
        assert!(second_session_text.contains("daemon"));
        assert!(second_session_text.contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn render_marks_only_one_selected_row() {
        let frame = render(30, 100, &workspace());
        let cursor_rows = frame
            .iter()
            .filter(|line| {
                let trimmed = strip(line).trim_start().to_owned();
                trimmed.starts_with('>') || trimmed.starts_with('\u{f0907}')
            })
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
                .any(|line| line.contains("\u{f0907} main"))
        );
        for expected in std::iter::once("main".to_string())
            .chain((0..12).map(|index| format!("session-{index:02}")))
        {
            let frame = render(8, 100, &ws);
            let selected = frame
                .iter()
                .map(|line| strip(line))
                .find(|line| {
                    let trimmed = line.trim_start();
                    trimmed.starts_with('>') || trimmed.starts_with('\u{f0907}')
                })
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

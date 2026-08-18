//! Workspace 画面（ホーム）。
//!
//! workspace を開いている間の主画面。全幅の **header** の下を 2 ペインに割る:
//!
//! - 左ペイン **session menu** — セッション一覧（session）・作成 action・キー操作の footer。
//! - 右ペイン **closeup** — フォーカス中セッションの header・タブ切替の tabmenu・content・footer。
//!
//! 状態 [`Workspace`] は core の workspace と永続化済み [`WorkspaceState`] から構築する、端末 IO を
//! 持たない純粋な値である。[`render`] が 1 フレーム分の行（ANSI 付き `Vec<String>`）に変換する。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use usagi_core::domain::agent::{
    AgentInventory, AgentRuntimeInventoryState, ProviderResumeProjection, ProviderResumeReason,
};
use usagi_core::domain::pullrequest::PrLink;
use usagi_core::domain::session::SessionRecord;
use usagi_core::domain::session_lifecycle::{
    AgentPhase, FailureStage, SessionLifecycle, SessionLifecycleProjection,
};
use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
use usagi_core::domain::workspace_state::WorkspaceState;
use usagi_core::usecase::client::{AgentConcurrency, DaemonMetrics};
use usagi_core::usecase::daemon_health::{
    DaemonHealth, DaemonHealthTracker, HealthLevel, HealthReason,
};
use usagi_core::usecase::session_state::SessionStateCounts;

use crate::presentation::frame::TERMINAL_CURSOR_MARKER;
use crate::presentation::layouts::panes;
use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::views::closeup_modal::{self, CloseupModal};
use crate::presentation::views::daemon_modal;
use crate::presentation::views::decision_modal;
use crate::presentation::views::director_drawer::{self, DIRECTOR_ICON, DirectorDrawerProjection};
use crate::presentation::views::overview_modal::{self, OverviewModal};
use crate::presentation::views::pr_modal::{self, PrModal};
use crate::presentation::views::text_overlay::{self, OverlayDocument, TextOverlay};
use crate::presentation::widgets;
pub use crate::presentation::widgets::live_terminal::TerminalViewProjection;
use crate::usecase::application::controller::{
    AppState, CreateSessionForm, Feedback, GardenClick, HomeMode, Notice, PrOverlay,
    PreviewOverlay, Selection, SessionRoleProjection, Target, TargetPhase,
};
use crate::usecase::application::pane::{
    PaneKind, PaneSelection, PaneState, PaneTab, TabSelection,
};
use crate::usecase::application::terminal_selection::TerminalPoint;
use usagi_core::domain::id::SessionId;

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
/// Nerd Font cogs: the Agent concurrency slots the daemon admits from.
const AGENT_ICON: char = '\u{f085}';
/// daemon が Agent concurrency を報告しない場合の表示。`0` と読み違えられない
/// 1 文字にするため em dash を使う。
const UNREPORTED: char = '—';
/// health indicator の警告記号。**Nerd Font ではなく** BMP の U+26A0 で、mascot の
/// speech bubble（[`abnormal_daemon_speech`]）と同じ語彙を使う。
const HEALTH_GLYPH: char = '\u{26a0}';
/// sidecar が始まるまでに mascot block が使う桁数（indent 1 + うさぎ 10 + 間隔 4）。
/// health badge はこれを引いた残りにだけ書き、狭幅では段階的に縮退する。
const SIDECAR_GUTTER: usize = 15;
const MEBIBYTE: u64 = 1_048_576;
const GIBIBYTE: u64 = 1_073_741_824;

/// Returns the PTY viewport that is visible inside the right-hand pane.
#[must_use]
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
    /// Safe interrupted/resume projection; provider-native IDs never enter the
    /// presentation model.
    pub agent_resume: Option<ProviderResumeProjection>,
    /// Daemon-authoritative lifecycle for this row. The sidebar derives per-row
    /// affordances from [`SessionLifecycle::capabilities`] (a `Failed` row is not
    /// attachable but is removable) and, for `Failed`, shows
    /// [`failure_summary`](Self::failure_summary).
    pub lifecycle: SessionLifecycle,
    /// Typed failure stage used for compact delete-failure recovery UI.
    pub failure_stage: Option<FailureStage>,
    /// Safe failure summary shown on a `Failed` row; `None` for other lifecycles.
    pub failure_summary: Option<String>,
    /// Safe display-only role ID from the daemon projection.
    pub role_id: Option<String>,
}

/// Nerd Font pull-request glyph shared with v1's right-aligned sidebar badge.
const PR_ICON: char = '\u{ea64}'; // nf-cod-git_pull_request
/// Keep the common one-digit badge column stable even before a PR is detected.
const PR_RESERVE_WIDTH: usize = 3;

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
            agent_resume: None,
            // Lifecycle is daemon-authoritative and joined by stable ID in
            // `project_controller_sessions`; a record with no snapshot lifecycle
            // (e.g. the non-interactive Home fallback) defaults to `Available`,
            // the only state those legacy paths ever projected.
            lifecycle: SessionLifecycle::Available,
            failure_stage: None,
            failure_summary: None,
            role_id: None,
        }
    }
}

pub(crate) fn pr_summary(prs: &[PrLink]) -> Option<String> {
    let visible = prs.iter().filter(|pr| pr.is_visible()).count();
    (visible > 0).then(|| format!("{PR_ICON} {visible}"))
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GardenMotion {
    Full,
    Reduced,
}

impl GardenMotion {
    const fn is_reduced(self) -> bool {
        matches!(self, Self::Reduced)
    }
}

/// controller の Home state を描画可能な session / action row へ投影した値。
///
/// session の順番は controller snapshot の `SessionId` 順を使い、表示情報は ID で結合する。
/// そのため表示名や入力 `Vec` の index を identity として扱わない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeProjection {
    workspace_name: String,
    sessions: Arc<[ProjectedSession]>,
    selected: Selection,
    active: Option<SessionId>,
    /// Session drawn in the right pane. Switch previews the row under the
    /// cursor, including while Director is open; every other case shows the
    /// command target ([`preview_session`]).
    preview: Option<SessionId>,
    mode: HomeMode,
    /// Phase line of the previewed session, not of the command target: the right
    /// pane is one surface and must not mix two sessions' material.
    preview_phase: TargetPhase,
    /// 表示中 session の running / waiting / failed 件数。daemon 権威の lifecycle と
    /// Agent phase 集約から [`SessionStateCounts::tally`] で毎フレーム導出する派生値で、
    /// `DaemonMetrics` にも controller state にも別の情報源を作らない。
    session_states: SessionStateCounts,
    feedback: Option<Feedback>,
    mascot_tick: u64,
    /// Presentation-only message. Runtime state currently supplies `None`; this
    /// keeps a future event source out of the renderer and prevents dummy copy.
    mascot_speech: Option<widgets::mascot::MascotSpeech>,
    /// 最新の daemon observation。毎フレーム外部から与える描画素材で、controller
    /// state（reducer）には持たせない。`None` は metrics 導入前と同じ静かな mascot を保つ。
    metrics: Option<DaemonMetrics>,
    /// daemon health の観測器。**診断専用の描画素材**で、reducer state にも操作の
    /// 権威にもならない。既定値（一度も観測していない）は indicator を出さないため、
    /// 正常時の frame は health 導入前と同一である。
    health: DaemonHealthTracker,
    /// sidebar の git 差分列。stable `SessionId` で session 行に結合する非永続の描画素材で、
    /// controller state には持たせない。空なら差分列を描かず metrics 導入前の frame を保つ。
    git_diffs: Arc<BTreeMap<SessionId, GitDiff>>,
    /// 選択中 live terminal の viewport 素材。`None` は live terminal 非表示で、右ペインは
    /// 既存の pane strip をそのまま描く。
    terminal_view: Option<Arc<TerminalViewProjection>>,
    pane_tabs: Vec<HomePaneTab>,
    pane_error: Option<String>,
    /// Non-sensitive detail of the selected interrupted Agent tab (#510). It
    /// replaces the phase line while a read-only history tab is selected.
    pane_detail: Option<String>,
    /// Whether the Closeup action modal covers the right pane this frame. Its
    /// final value is only known once [`Self::with_pane`] has seen the pane
    /// strip: the modal is the launcher surface only while Closeup has no tab at
    /// all (pending included). An explicit or forced [`Overlay::Closeup`] keeps
    /// it visible regardless, so [`Self::from_state`] seeds that case here and
    /// [`Self::with_pane`] adds the empty-pane launcher case.
    closeup_action_visible: bool,
    /// Whether the controller route is Closeup. [`Self::with_pane`] pairs this
    /// with an empty pane strip to reveal the action launcher; it never widens
    /// the explicit-overlay case seeded by [`Self::from_state`].
    closeup_route: bool,
    decision_overlay: Option<crate::usecase::application::controller::DecisionOverlayState>,
    decisions: Vec<usagi_core::domain::user_decision::UserDecision>,
    unread_decision_ids: std::collections::BTreeSet<usagi_core::domain::id::UserDecisionId>,
    /// Open Pull Request overlay projection, drawn above the sidebar/pane frame.
    pr_overlay: Option<PrOverlay>,
    /// Open Markdown preview overlay projection, drawn above the frame.
    preview_overlay: Option<PreviewOverlay>,
    /// Persisted Overview command-palette input, when its overlay is open. The
    /// runtime owns this so the caret and filter survive across frames.
    overview_modal: Option<OverviewModal>,
    /// Overview の `daemon` command が開く読み取り専用 status surface。
    daemon_overlay: bool,
    /// Garden の描画素材。overlay が閉じている間は `None` で、開いている frame だけ
    /// session と、それに属する runtime-local phase を庭の projection へ写す。
    garden_sessions: Option<Vec<widgets::garden::GardenSession>>,
    /// Composition root が一度だけ解決した Garden の motion preference。
    garden_motion: GardenMotion,
    /// Latest coherent daemon Agent inventory projected to safe display rows.
    daemon_runtimes: Option<Vec<daemon_modal::AgentRuntimeRow>>,
    /// Persisted Closeup action-modal input, when its overlay is open.
    closeup_modal: Option<CloseupModal>,
    /// Inline `+ new session` name draft, present exactly when the create form
    /// owns input. The sidebar row renders it as a name-only caret in place of the
    /// static `+ new session` label; profile/model are never part of this flow.
    create_draft: Option<CreateDraft>,
    create_role: Option<String>,
    /// Name of a create request the daemon is still fulfilling. Present exactly
    /// while a create worker owns the port; the sidebar draws it as a two-line
    /// loading skeleton just above `+ new session` (`document/03-tui.md`) until
    /// the daemon's `session.created` row replaces it.
    create_pending: Option<String>,
    /// Frontmost Director mode drawer material. The empty default is the only
    /// projection currently connected; later runtime work can populate its
    /// conversation selector and terminal rows through
    /// [`Self::with_director_drawer`].
    director_drawer: Option<DirectorDrawerProjection>,
}

/// Left-sidebar draft for the inline new-session input.
///
/// A projection of the controller's `CreateSessionForm`: just the typed name and
/// any validation message the reducer attached. It replaces the former centered
/// three-field modal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateDraft {
    name: String,
    error: Option<String>,
}

impl From<&CreateSessionForm> for CreateDraft {
    fn from(form: &CreateSessionForm) -> Self {
        Self {
            name: form.name().to_owned(),
            error: form.error().map(clone_notice_message),
        }
    }
}

fn clone_notice_message(notice: &Notice) -> String {
    notice.message.clone()
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
    /// 側の snapshot reconciliation が selected / active を surviving session
    /// または `+ new session` / active なしへ縮退させる。
    #[must_use]
    pub fn from_state(
        state: &AppState,
        workspace_name: &str,
        root_cwd: &Path,
        snapshot_sessions: &[ProjectedSession],
    ) -> Self {
        let _ = root_cwd;
        // Build one stable-identity index, then retain the controller's order.
        // The former nested scan made a rebuild O(state sessions × snapshot
        // sessions), even though both sides already carry the authoritative ID.
        let snapshot_by_id = snapshot_sessions
            .iter()
            .map(|session| (session.id, session))
            .collect::<HashMap<_, _>>();
        let sessions = state
            .sessions()
            .iter()
            .filter_map(|id| {
                let mut session = (*snapshot_by_id.get(id)?).clone();
                if let Some(prs) = state.session_prs(*id) {
                    session.pr_summary = pr_summary(prs);
                }
                session.role_id = state
                    .session_roles()
                    .get(id)
                    .and_then(|projection| projection.role_id.as_ref())
                    .map(ToString::to_string);
                Some(session)
            })
            .collect::<Vec<_>>();
        Self::from_ordered_state(state, workspace_name, Arc::from(sessions))
    }

    /// Build from an already ordered, stable-ID joined row projection. This is
    /// the frame loop's change-driven path: unrelated metrics, clock, overlay or
    /// terminal updates retain the same owned rows without cloning label/path.
    pub(crate) fn from_ordered_state(
        state: &AppState,
        workspace_name: &str,
        sessions: Arc<[ProjectedSession]>,
    ) -> Self {
        let create_draft = state.create_session_form().map(CreateDraft::from);
        let create_role = state
            .create_session_form()
            .and_then(CreateSessionForm::selected_role)
            .map(|role| role.id.to_string());
        let preview = preview_session(state);
        // Derived before the sessions move into the projection: the counts are a
        // fold of the same daemon-authoritative rows, not a second source.
        let session_states = session_state_counts(state, &sessions);
        // Garden が開いている frame だけ庭の projection を作る。閉じている間は素材を
        // 持たないので、通常の Home frame は Garden 導入前と同じ経路で描かれる。
        let garden_sessions = (state.overlay()
            == Some(crate::usecase::application::controller::Overlay::Garden))
        .then(|| {
            sessions
                .iter()
                .map(|session| widgets::garden::GardenSession {
                    id: session.id,
                    label: session.label.clone(),
                    lifecycle: session.lifecycle,
                    selected: state.selected() == Selection::Target(Target::Session(session.id)),
                    failure_summary: session.failure_summary.clone(),
                    pr_merged: state.celebrates_pr_merge(session.id),
                    agents: state
                        .runtimes()
                        .iter()
                        .filter(|entry| entry.runtime.session_id == Some(session.id))
                        .map(|entry| widgets::garden::GardenAgent {
                            runtime_id: entry.runtime.agent_runtime_id,
                            phase: garden_phase(entry.phase),
                        })
                        .collect(),
                })
                .collect::<Vec<_>>()
        });
        Self {
            workspace_name: workspace_name.to_owned(),
            sessions,
            selected: state.selected(),
            active: state.active(),
            preview,
            mode: match state.route() {
                crate::usecase::application::controller::Route::Home(mode) => mode,
            },
            preview_phase: preview.map_or(TargetPhase::Absent, |session| {
                state.phase_for(Target::Session(session))
            }),
            session_states,
            feedback: state.feedback().cloned(),
            mascot_tick: state.mascot_tick(),
            mascot_speech: None,
            metrics: None,
            health: DaemonHealthTracker::default(),
            git_diffs: Arc::new(BTreeMap::new()),
            terminal_view: None,
            pane_tabs: Vec::new(),
            pane_error: None,
            pane_detail: None,
            // Seed only the explicit/forced action modal here (an open
            // `Overlay::Closeup`). The launcher-over-empty-pane case cannot be
            // decided without the pane strip, so `with_pane` finalizes it; this
            // keeps a pending launch from being covered every frame.
            closeup_action_visible: matches!(
                state.route(),
                crate::usecase::application::controller::Route::Home(HomeMode::Closeup)
            ) && state.overlay()
                == Some(crate::usecase::application::controller::Overlay::Closeup),
            closeup_route: matches!(
                state.route(),
                crate::usecase::application::controller::Route::Home(HomeMode::Closeup)
            ),
            decision_overlay: state.decision_overlay().cloned(),
            decisions: state.decisions().to_vec(),
            unread_decision_ids: state.unread_decision_ids().clone(),
            pr_overlay: state.pr_overlay().cloned(),
            preview_overlay: state.preview_overlay().cloned(),
            overview_modal: None,
            daemon_overlay: state.overlay()
                == Some(crate::usecase::application::controller::Overlay::Daemon),
            garden_sessions,
            garden_motion: GardenMotion::Full,
            daemon_runtimes: None,
            closeup_modal: None,
            create_draft,
            create_role,
            // Seeded by the shell's in-flight create via `with_create_pending`;
            // the reducer never owns the pending name because its snapshot arrives
            // through the daemon transport, not `AppState`.
            create_pending: None,
            director_drawer: state
                .director_drawer_open()
                .then(DirectorDrawerProjection::default),
        }
    }

    /// Attach the composition-owned motion preference without teaching the
    /// renderer or reducer to read process environment.
    #[must_use]
    pub fn with_garden_reduced_motion(mut self, reduced_motion: bool) -> Self {
        self.garden_motion = if reduced_motion {
            GardenMotion::Reduced
        } else {
            GardenMotion::Full
        };
        self
    }

    pub(crate) fn canonical_garden_now(
        &self,
        raw_height: usize,
        raw_width: usize,
        now: DateTime<Utc>,
    ) -> DateTime<Utc> {
        let Some(sessions) = self.garden_sessions.as_deref() else {
            return now;
        };
        let (height, width) = widgets::normalize_size(raw_height, raw_width);
        let Some(tick) = widgets::garden::canonical_tick(
            height,
            width,
            sessions,
            garden_tick(now),
            self.garden_motion.is_reduced(),
        ) else {
            return now;
        };
        DateTime::from_timestamp(i64::try_from(tick).expect("Garden phases fit i64"), 0)
            .expect("a Garden-cycle Unix timestamp is valid")
    }

    /// Attach the name of a create request the daemon is still fulfilling, so the
    /// sidebar draws its loading skeleton. `None` while no create worker owns the
    /// port.
    #[must_use]
    pub fn with_create_pending(mut self, name: Option<String>) -> Self {
        self.create_pending = name;
        self
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
        self.pane_detail = pane
            .tabs()
            .iter()
            .find(|tab| pane_tab_selected(tab, pane.selected()))
            .and_then(|tab| match tab {
                PaneTab::Interrupted(interrupted) => Some(interrupted_detail(interrupted)),
                PaneTab::Pending(_) | PaneTab::Live(_) | PaneTab::Ready(_) => None,
            });
        // In Closeup the action modal is the launcher shown only while the pane
        // holds no tab at all — pending placeholders included. A pending launch
        // therefore keeps the wave visible instead of being re-covered every
        // frame; the explicit/forced overlay case is already seeded above and is
        // never narrowed here.
        if self.closeup_route && self.pane_tabs.is_empty() {
            self.closeup_action_visible = true;
        }
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

    /// Attach the latest coherent daemon Agent inventory to Garden and the
    /// status modal. Runtime identity is shortened only for the modal; Garden
    /// joins by stable `SessionId` / `AgentRuntimeId`, never visible labels.
    #[must_use]
    pub fn with_agent_inventory(mut self, inventory: Option<&AgentInventory>) -> Self {
        if let (Some(garden_sessions), Some(inventory)) = (self.garden_sessions.as_mut(), inventory)
        {
            for item in &inventory.runtimes {
                let Some(session_id) = item.runtime.session_id else {
                    // Workspace-root runtimes have no session plot.
                    continue;
                };
                let Some(session) = garden_sessions
                    .iter_mut()
                    .find(|session| session.id == session_id)
                else {
                    // A stale or foreign session cannot create a new plot.
                    continue;
                };
                if session
                    .agents
                    .iter()
                    .any(|agent| agent.runtime_id == item.runtime.agent_runtime_id)
                {
                    // Runtime-local phase pushes are more precise than the
                    // coarse durable inventory state (notably Waiting).
                    continue;
                }
                session.agents.push(widgets::garden::GardenAgent {
                    runtime_id: item.runtime.agent_runtime_id,
                    phase: garden_inventory_phase(item.state),
                });
            }
        }
        self.daemon_runtimes = inventory.map(|inventory| {
            inventory
                .runtimes
                .iter()
                .map(|item| {
                    let scope = item.runtime.session_id.map_or_else(
                        || "root".to_owned(),
                        |session_id| {
                            self.sessions
                                .iter()
                                .find(|session| session.id == session_id)
                                .map_or_else(
                                    || format!("session #{}", short_id(&session_id.to_string())),
                                    |session| session.label.clone(),
                                )
                        },
                    );
                    daemon_modal::AgentRuntimeRow {
                        scope,
                        runtime_id: short_id(&item.runtime.agent_runtime_id.to_string()),
                        state: item.state,
                    }
                })
                .collect()
        });
        self
    }

    /// Attach the diagnostic daemon-health observer for the sidecar indicator.
    ///
    /// It is draw material only: no operation, ownership decision, or reducer
    /// event reads it. The default (nothing observed) draws no indicator, so a
    /// healthy or daemon-less workspace keeps the frame it has today.
    #[must_use]
    pub const fn with_health(mut self, health: DaemonHealthTracker) -> Self {
        self.health = health;
        self
    }

    /// Attach the asynchronously refreshed Git observations drawn as the sidebar
    /// diff columns without touching controller or input state. The diffs join to
    /// session rows by stable `SessionId`; an empty map leaves the sidebar in its
    /// pre-diff form.
    #[must_use]
    pub fn with_git_diffs(mut self, diffs: &BTreeMap<SessionId, GitDiff>) -> Self {
        self.git_diffs = Arc::new(diffs.clone());
        self
    }

    #[must_use]
    pub(crate) fn with_shared_git_diffs(
        mut self,
        diffs: Arc<BTreeMap<SessionId, GitDiff>>,
    ) -> Self {
        self.git_diffs = diffs;
        self
    }

    /// Attach the focused live terminal's viewport rows, scroll offset and
    /// feedback for the right pane without touching controller or input state.
    /// `None` keeps the right pane on its existing tab strip.
    #[must_use]
    pub fn with_terminal_view(mut self, view: Option<TerminalViewProjection>) -> Self {
        self.terminal_view = view.map(Arc::new);
        self
    }

    #[must_use]
    pub(crate) fn with_shared_terminal_view(
        mut self,
        view: Option<Arc<TerminalViewProjection>>,
    ) -> Self {
        self.terminal_view = view;
        self
    }

    /// Borrow the focused live terminal projection for a shell-owned overlay
    /// that needs to rebuild the current Home frame.
    #[must_use]
    pub(crate) fn terminal_view(&self) -> Option<&TerminalViewProjection> {
        self.terminal_view.as_deref()
    }

    /// Replace the open drawer's presentation material without changing its
    /// controller-owned open/closed state. A closed drawer ignores supplied
    /// material, keeping runtime inventory from opening UI implicitly.
    #[must_use]
    pub fn with_director_drawer(mut self, projection: DirectorDrawerProjection) -> Self {
        if self.director_drawer.is_some() {
            self.director_drawer = Some(projection);
        }
        self
    }

    /// Collapse the animation clock onto the frame it actually draws.
    ///
    /// `mascot_tick` advances on every 16ms tick, so comparing two projections
    /// for render equality would always disagree even on a completely idle
    /// Home. Only four surfaces read the clock: the sidebar rabbit, the removal
    /// shimmer, a pending tab's wave, and the create skeleton. The last three
    /// exist only while their subject does, so when none of them is on screen
    /// the clock can be folded onto the rabbit's representative tick and the
    /// shell can skip the redraw (#554). The rabbit's own cadence is unchanged:
    /// [`widgets::mascot::canonical_tick`] keeps every visibly distinct phase.
    ///
    /// Call this last — after every `with_*` step — because the pending tab and
    /// the create skeleton only become visible through those steps.
    #[must_use]
    pub fn collapse_animation_clock(mut self) -> Self {
        let drives_a_per_tick_animation = self.create_pending.is_some()
            || self.sessions.iter().any(|session| session.removing)
            || self.pane_tabs.iter().any(|tab| tab.pending);
        if !drives_a_per_tick_animation {
            self.mascot_tick = widgets::mascot::canonical_tick(self.mascot_tick);
        }
        self
    }

    /// 左 sidebar の rows。managed sessions の末尾に `+ new session` を置く。
    #[must_use]
    pub fn rows(&self) -> Vec<Selection> {
        let mut rows = Vec::with_capacity(self.sessions.len() + 1);
        rows.extend(
            self.sessions
                .iter()
                .map(|session| Selection::Target(Target::Session(session.id))),
        );
        rows.push(Selection::NewSession);
        rows
    }

    /// Whether the right pane owns keyboard input on this frame.
    ///
    /// Only a Closeup route whose selected tab is a live terminal, with no
    /// foreground surface over it, receives input. Every other frame leaves the
    /// pane's scroll, tab, selection, and copy controls inert, so the pane is
    /// drawn dim to say so: Switch (the sidebar navigates), a pending or
    /// interrupted tab (no live terminal), an open overlay or action modal, and
    /// an open Director drawer (its root conversation owns input).
    fn right_pane_focused(&self) -> bool {
        self.mode == HomeMode::Closeup
            && self.terminal_view.is_some()
            && self.director_drawer.is_none()
            && !self.closeup_action_visible
            && self.overview_modal.is_none()
            && self.pr_overlay.is_none()
            && self.preview_overlay.is_none()
            && self.decision_overlay.is_none()
    }

    fn active_label(&self) -> &str {
        self.session_label(self.active)
    }

    /// Label of the session the right pane previews. Switch names the row under
    /// the cursor; Closeup names the target it operates on.
    fn preview_label(&self) -> &str {
        self.session_label(self.preview)
    }

    fn session_label(&self, session: Option<SessionId>) -> &str {
        match session {
            Some(id) => self
                .sessions
                .iter()
                .find(|session| session.id == id)
                .map_or("No session selected", |session| session.label.as_str()),
            None => "No session selected",
        }
    }
}

/// The session the right pane previews.
///
/// Switch owns sidebar navigation, so its right pane follows the cursor: moving
/// it is how the user looks at another session before choosing to act on it.
/// Closeup and the `+ new session` action row show the command target instead.
/// Opening Director does not change this background projection: the drawer is a
/// foreground surface, so the visible Switch pane continues to describe the
/// selected row. `WorkspaceRuntime::preview_pane` resolves the same session so
/// the tab strip and the header name one session.
fn preview_session(state: &AppState) -> Option<SessionId> {
    let crate::usecase::application::controller::Route::Home(mode) = state.route();
    if mode == HomeMode::Switch
        && let Selection::Target(Target::Session(session)) = state.selected()
    {
        return Some(session);
    }
    state.active()
}

fn pane_tab_label(tab: &PaneTab) -> String {
    match tab {
        // Interrupted history is labelled from the closed provider vocabulary
        // only, so no provider-native identity can reach the tab strip.
        PaneTab::Interrupted(pane) => match pane.resuming {
            Some(_) => format!(
                "{} (resuming)",
                crate::usecase::application::interrupted_tab::provider_label(pane.tab.provider)
            ),
            None => pane.tab.safe_label(),
        },
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

fn pane_tab_selected(tab: &PaneTab, selection: &PaneSelection) -> bool {
    // Each tab kind carries its own stable selection key, so comparing keys is
    // enough: a mismatched kind or identity simply compares unequal.
    *selection == PaneSelection::Tab(pane_tab_selection(tab))
}

fn pane_tab_selection(tab: &PaneTab) -> TabSelection {
    match tab {
        PaneTab::Pending(pending) => TabSelection::Pending(pending.operation),
        PaneTab::Live(live) => TabSelection::Live(live.terminal.clone()),
        PaneTab::Ready(ready) => TabSelection::Ready(ready.operation),
        PaneTab::Interrupted(pane) => TabSelection::Interrupted(pane.tab.continuation),
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

    fn label(self) -> &'static str {
        match self {
            Self::Switch => "Switch",
            Self::Closeup => "Closeup",
        }
    }

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
    /// Daemon-authoritative lifecycle projection by stable ID. Non-persistent;
    /// refreshed from each lifecycle snapshot. A session absent here (older
    /// snapshot or a name-only fallback) is treated as `Available`.
    session_lifecycles: BTreeMap<SessionId, SessionLifecycleProjection>,
    /// Non-persistent safe role metadata from the daemon snapshot.
    session_roles: BTreeMap<SessionId, SessionRoleProjection>,
    /// Monotonic generation of daemon-derived row material. It is a cache key,
    /// never an authority; the records and stable IDs above remain the `SSoT`.
    material_revision: u64,
}

impl Workspace {
    /// core の workspace とその永続化済み状態からセッションキャッシュを作る。
    #[must_use]
    pub fn new(workspace: WorkspaceRecord, state: WorkspaceState) -> Self {
        let mut session_ids = Vec::with_capacity(state.sessions.len());
        session_ids.resize_with(state.sessions.len(), SessionId::new);
        Self::with_runtime_ids(workspace, state, session_ids)
    }

    /// Build the cache from daemon-authoritative workspace state and session
    /// identities. The identities fence pane requests and completions.
    #[must_use]
    pub fn with_runtime_ids(
        workspace: WorkspaceRecord,
        state: WorkspaceState,
        session_ids: Vec<SessionId>,
    ) -> Self {
        let session_ids = if session_ids.len() == state.sessions.len() {
            session_ids
        } else {
            let mut fallback_ids = Vec::with_capacity(state.sessions.len());
            fallback_ids.resize_with(state.sessions.len(), SessionId::new);
            fallback_ids
        };
        Self {
            record: workspace,
            state,
            session_ids,
            metrics: None,
            git_diffs: BTreeMap::new(),
            session_lifecycles: BTreeMap::new(),
            session_roles: BTreeMap::new(),
            material_revision: 0,
        }
    }

    /// workspace 名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.record.name
    }

    /// workspace の絶対パス。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.record.path
    }

    /// セッション一覧。
    #[must_use]
    pub fn sessions(&self) -> &[SessionRecord] {
        &self.state.sessions
    }

    /// Daemon identities aligned with [`Self::sessions`].
    #[must_use]
    pub fn session_ids(&self) -> &[SessionId] {
        &self.session_ids
    }

    /// Replace only the sidebar's session projection from a daemon lifecycle
    /// snapshot. The persisted workspace state remains read-only auxiliary data.
    pub fn replace_sessions(&mut self, sessions: Vec<SessionRecord>) {
        self.replace_sessions_and_ids(sessions, None);
    }

    /// Replace sidebar rows and their daemon-issued runtime identities from one
    /// lifecycle snapshot. The vectors are aligned by snapshot order; names
    /// remain display-only and are never used to recover an identity.
    pub fn replace_sessions_with_runtime_ids(
        &mut self,
        sessions: Vec<SessionRecord>,
        session_ids: Vec<SessionId>,
    ) {
        self.replace_sessions_and_ids(sessions, Some(session_ids));
    }

    fn replace_sessions_and_ids(
        &mut self,
        sessions: Vec<SessionRecord>,
        session_ids: Option<Vec<SessionId>>,
    ) {
        let changed = self.state.sessions != sessions
            || session_ids
                .as_ref()
                .is_some_and(|ids| self.session_ids != *ids);
        self.state.sessions = sessions;
        if let Some(session_ids) = session_ids {
            debug_assert_eq!(session_ids.len(), self.state.sessions.len());
            self.session_ids = session_ids;
        }
        if changed {
            self.material_revision = self.material_revision.saturating_add(1);
        }
    }

    /// Replaces the daemon-observed metrics shown in the sidebar footer area.
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
    pub fn set_git_diffs(&mut self, diffs: BTreeMap<SessionId, GitDiff>) {
        self.git_diffs = diffs;
    }

    /// The completed Git observations keyed by session, for the controller
    /// `HomeProjection::with_git_diffs` projection.
    #[must_use]
    pub fn git_diffs(&self) -> &BTreeMap<SessionId, GitDiff> {
        &self.git_diffs
    }

    /// Replace the daemon-authoritative lifecycle projection keyed by stable ID.
    pub fn set_session_lifecycles(
        &mut self,
        lifecycles: BTreeMap<SessionId, SessionLifecycleProjection>,
    ) {
        if self.session_lifecycles != lifecycles {
            self.session_lifecycles = lifecycles;
            self.material_revision = self.material_revision.saturating_add(1);
        }
    }

    /// The lifecycle projection keyed by stable ID, joined onto each sidebar row
    /// in `project_controller_sessions`.
    #[must_use]
    pub fn session_lifecycles(&self) -> &BTreeMap<SessionId, SessionLifecycleProjection> {
        &self.session_lifecycles
    }

    pub fn set_session_roles(&mut self, roles: BTreeMap<SessionId, SessionRoleProjection>) {
        if self.session_roles != roles {
            self.session_roles = roles;
            self.material_revision = self.material_revision.saturating_add(1);
        }
    }

    #[must_use]
    pub fn session_roles(&self) -> &BTreeMap<SessionId, SessionRoleProjection> {
        &self.session_roles
    }

    #[must_use]
    pub const fn material_revision(&self) -> u64 {
        self.material_revision
    }

    /// The workspace record passed to the daemon lifecycle command port.
    #[must_use]
    pub fn record(&self) -> &WorkspaceRecord {
        &self.record
    }
}

// ── header ──────────────────────────────────────────────────────────────────

/// v1 の chrome と同じアイコン付き mode 表示。現在の mode だけを accent で強調する。
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

/// Clickable Home-header actions. Rendering and hit-testing are projected by the
/// same [`HomeHeaderLayout`], so notice/button ranges cannot drift from CJK or
/// narrow-width clipping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeHeaderAction {
    Director,
    Decisions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HomeHeaderLayout {
    line: String,
    actions: Vec<(HomeHeaderAction, std::ops::Range<usize>)>,
}

impl HomeHeaderLayout {
    fn action_at(&self, column: usize) -> Option<HomeHeaderAction> {
        self.actions
            .iter()
            .find_map(|(action, range)| range.contains(&column).then_some(*action))
    }
}

/// Resolve a header click with the exact layout used by [`render_home`].
#[must_use]
pub fn home_header_action_at(
    width: usize,
    home: &HomeProjection,
    column: u16,
    row: u16,
) -> Option<HomeHeaderAction> {
    (row == 0)
        .then(|| home_header_layout(width, home).action_at(usize::from(column)))
        .flatten()
}

fn home_header_layout(width: usize, home: &HomeProjection) -> HomeHeaderLayout {
    let mode = match home.mode {
        HomeMode::Switch => Mode::Switch,
        HomeMode::Closeup => Mode::Closeup,
    };
    let breadcrumb = format!(
        " {}{}{}",
        Role::Success.style().bold().paint("USAGI"),
        Style::new().dim().paint(" > "),
        Role::Success.style().bold().paint(&home.workspace_name),
    );
    // The drawer button reads as one more entry in the same right-hand strip as
    // the mode toggle, so it follows the toggle's active/inactive contrast: the
    // accent belongs to whatever is currently in front. A closed drawer is dim
    // like an unselected mode; an open one takes the accent plus reverse so the
    // frontmost surface is unambiguous.
    let director = if home.director_drawer.is_some() {
        Role::Accent
            .style()
            .bold()
            .reverse()
            .paint(&format!("[ {DIRECTOR_ICON} director ]"))
    } else {
        Style::new()
            .dim()
            .paint(&format!("[ {DIRECTOR_ICON} director ]"))
    };
    let notice = (!home.unread_decision_ids.is_empty())
        .then(|| format!("🔔 {} notice", home.unread_decision_ids.len()));
    let mode = mode_toggle(mode);

    // Preserve the drawer entry first, then the mode indicator, then the notice.
    // Whole optional segments are admitted only when they fit; the breadcrumb
    // receives the remaining cells and is the only normal-width clipped field.
    let mut right_segments = vec![(Some(HomeHeaderAction::Director), director)];
    let mut used = widgets::display_width(&right_segments[0].1);
    let mode_width = widgets::display_width(&mode);
    if used.saturating_add(2).saturating_add(mode_width) <= width {
        right_segments.insert(0, (None, mode));
        used += 2 + mode_width;
    }
    if let Some(notice) = notice {
        let notice_width = widgets::display_width(&notice);
        if used.saturating_add(2).saturating_add(notice_width) <= width {
            right_segments.insert(0, (Some(HomeHeaderAction::Decisions), notice));
            used += 2 + notice_width;
        }
    }

    if used > width {
        let button = widgets::clip_to_width(&right_segments.last().expect("button").1, width);
        let button_width = widgets::display_width(&button);
        return HomeHeaderLayout {
            line: widgets::pad_to_width(&button, width),
            actions: (button_width > 0)
                .then_some((HomeHeaderAction::Director, 0..button_width))
                .into_iter()
                .collect(),
        };
    }

    let right_width = used;
    let left_width = width.saturating_sub(right_width);
    let mut line =
        widgets::pad_to_width(&widgets::clip_to_width(&breadcrumb, left_width), left_width);
    let mut actions = Vec::new();
    let mut cursor = left_width;
    for (index, (action, segment)) in right_segments.into_iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
            cursor += 2;
        }
        let segment_width = widgets::display_width(&segment);
        if let Some(action) = action {
            actions.push((action, cursor..cursor + segment_width));
        }
        line.push_str(&segment);
        cursor += segment_width;
    }
    HomeHeaderLayout { line, actions }
}

/// Header の下に呼吸できる余白を作る全幅の空行。
fn header_spacer(width: usize) -> String {
    " ".repeat(width)
}

// ── left pane: session menu ─────────────────────────────────────────────────

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
    metadata: &str,
    diff: Option<&GitDiff>,
    columns: SidebarDiffColumns,
    pr: Option<&str>,
    pr_width: usize,
    width: usize,
    dim: bool,
) -> String {
    let diff_width = sidebar_git_summary_width(columns);
    let diff = if diff_width == 0 {
        String::new()
    } else {
        diff.map_or_else(
            || " ".repeat(diff_width),
            |diff| git_diff_text(diff, columns, dim),
        )
    };
    let pr_badge = pr.map(|pr| Role::Info.style().underline().paint(pr));
    let pr = pr_badge.as_ref().map_or_else(
        || " ".repeat(pr_width),
        |badge| {
            format!(
                "{}{}",
                " ".repeat(pr_width.saturating_sub(widgets::display_width(badge))),
                badge
            )
        },
    );
    let separator = usize::from(diff_width > 0 && pr_width > 0);
    let right_width = diff_width + separator + pr_width;
    let right = if separator == 0 {
        format!("{diff}{pr}")
    } else {
        format!("{diff} {pr}")
    };
    let available = width;
    if right_width > available {
        let priority = pr_badge.as_deref().unwrap_or(&diff);
        let clipped = widgets::clip_to_width(priority, available);
        let gap = available.saturating_sub(widgets::display_width(&clipped));
        return format!("{}{clipped}", " ".repeat(gap));
    }
    let prefix = widgets::clip_to_width(metadata, available.saturating_sub(right_width));
    let gap = available
        .saturating_sub(widgets::display_width(&prefix))
        .saturating_sub(right_width);
    format!("{prefix}{}{right}", " ".repeat(gap))
}

fn sidebar_pr_width(sessions: &[ProjectedSession]) -> usize {
    sessions
        .iter()
        .filter_map(|session| session.pr_summary.as_deref())
        .map(widgets::display_width)
        .max()
        .unwrap_or_default()
        .max(PR_RESERVE_WIDTH)
}

fn sidebar_git_summary_width(columns: SidebarDiffColumns) -> usize {
    if columns == SidebarDiffColumns::default() {
        return 0;
    }
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

/// Fold the joined Home rows into the workspace's running / waiting / failed
/// counts.
///
/// Both inputs are daemon-authoritative and already on this frame: the
/// per-session lifecycle joined onto [`ProjectedSession`] from the lifecycle
/// snapshot, and the session-scope Agent phase the controller aggregates from the
/// daemon's phase reports. The classification itself lives in
/// [`usagi_core::usecase::session_state`], beside the phase aggregation it reuses,
/// so the rule is stated once instead of in a view.
fn session_state_counts(state: &AppState, sessions: &[ProjectedSession]) -> SessionStateCounts {
    let classified = sessions
        .iter()
        .map(|session| {
            (
                session.lifecycle,
                state.phase_for(Target::Session(session.id)).aggregation(),
            )
        })
        .collect::<Vec<_>>();
    SessionStateCounts::tally(&classified)
}

/// The mascot sidecar line summarising session state, or `None` when there is
/// nothing to summarise.
///
/// A class with no session is omitted rather than drawn as `0`, and an entirely
/// quiet workspace (no session at all included) yields no line, so the sidecar
/// stays as short as the news it carries. The line is independent of
/// [`DaemonMetrics`]: it is drawn even while the daemon observation is
/// unavailable.
fn session_state_sidecar(counts: SessionStateCounts) -> Option<String> {
    if counts.is_empty() {
        return None;
    }
    // Role vocabulary is the theme's own: running is Success, waiting is
    // Warning, a failed checkout is Danger.
    let segments = [
        (counts.running, "run", Role::Success),
        (counts.waiting, "wait", Role::Warning),
        (counts.failed, "fail", Role::Danger),
    ]
    .into_iter()
    .filter(|(count, _, _)| *count > 0)
    .map(|(count, label, role)| role.style().paint(&format!("{label} {count}")))
    .collect::<Vec<_>>();
    Some(segments.join(" "))
}

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
            // The sidecar is bottom-aligned beside the rabbit, so the Agent
            // concurrency row goes *above* the process row: the CPU/memory line
            // keeps the bottom position it has drawn at since it was introduced,
            // and the session-state summary stays the topmost line.
            vec![
                agent_concurrency_row(metrics.agent_concurrency),
                format!("{cpu}  {memory}"),
            ]
        },
    )
}

/// The sidecar's Agent concurrency row: how many of the daemon's Agent
/// concurrency slots are in use, over the limit it admits from.
///
/// Both numbers come from the daemon's own admission authority
/// ([`DaemonMetrics::agent_concurrency`]); this view never counts runtimes itself
/// and never restates the daemon's limit. It is the **Agent** pool, not the
/// generic terminal capacity and not a supervisor run's concurrency.
///
/// `None` means the daemon reported nothing (a peer older than metrics schema 3),
/// which is drawn as a dash so it cannot be read as an idle `0`.
fn agent_concurrency_row(concurrency: Option<AgentConcurrency>) -> String {
    // The mascot row is pink, so a calm value sets white explicitly for the same
    // reason `load_style` does.
    let calm = Style::new().fg(Color::White).dim();
    match concurrency {
        None => calm.paint(&format!("{AGENT_ICON} {UNREPORTED}")),
        Some(concurrency) => {
            let style = if concurrency.is_saturated() {
                // The next Agent launch is refused, which is worth the strongest
                // colour the sidecar has.
                Role::Danger.style()
            } else if concurrency.reaches_fraction(3, 4) {
                Role::Warning.style()
            } else {
                calm
            };
            style.paint(&format!(
                "{AGENT_ICON} {}/{}",
                concurrency.in_use, concurrency.limit
            ))
        }
    }
}

/// mascot の sidecar に出す行を上から順に組む。
///
/// sidecar はうさぎの 3 行に対して最大 3 行を許すため、health・session 件数・metrics の
/// 3 行が揃っても mascot の予約行数（[`widgets::mascot::MascotBlock::reserved_rows`]）は
/// 変わらない。health の badge は「異常時だけ」の行なので最上段に置き、常設の
/// 件数・metrics 行の上へ載せる。**health が `Ok` のときの戻り値は health 導入前と同一である。**
fn sidecar_labels(
    width: usize,
    metrics: Option<&DaemonMetrics>,
    health: DaemonHealth,
    session_states: SessionStateCounts,
) -> Vec<String> {
    let badge = health_badge(health, width);
    // session 件数は lifecycle / phase projection だけから決まるので、daemon の観測が
    // 無くても出る。
    let counts = session_state_sidecar(session_states);
    // 観測が無いときは Agent concurrency 行も CPU / メモリ行も出さない（metrics 導入前と
    // 同じ静けさ）。health だけが非 Ok なら badge 行だけが出る。
    let mut metric_rows = metrics
        .map(|metrics| mascot_metrics(Some(metrics), 0))
        .unwrap_or_default();
    // 供給元は 4 つあるが、sidecar はうさぎの 3 行にしか載らない。4 つとも語ることが
    // あるときは **Agent concurrency 行が譲る**。異常な daemon の方が急を要する報せで
    // あり、ここで明示的に落としておかないと widget の `take(3)` が代わりに最下段の
    // CPU / メモリ行を無言で捨てる（[`widgets::mascot::sidebar_block_with_sidecar`]）。
    if badge.is_some() && counts.is_some() && metric_rows.len() > 1 {
        metric_rows.remove(0);
    }
    let mut labels = Vec::new();
    labels.extend(badge);
    labels.extend(counts);
    labels.extend(metric_rows);
    labels
}

/// daemon health の 1 行。診断だけを短く伝え、raw な出力・path・secret は載せない。
///
/// 幅が足りなければ記号 1 文字へ縮退し、記号すら置けない幅では行を出さない。
fn health_badge(health: DaemonHealth, width: usize) -> Option<String> {
    let reason = health.reason()?;
    let budget = width.saturating_sub(SIDECAR_GUTTER);
    let glyph = HEALTH_GLYPH.to_string();
    if budget < widgets::display_width(&glyph) {
        return None;
    }
    let text = format!("{HEALTH_GLYPH} {}", health_reason_label(reason));
    let label = if widgets::display_width(&text) <= budget {
        text
    } else {
        glyph
    };
    let style = if health.level() == HealthLevel::Danger {
        Role::Danger.style().bold()
    } else {
        Role::Warning.style().bold()
    };
    Some(style.paint(&label))
}

/// 閉じた理由 enum から表示文言へ。free text を通さないため、raw output は載り得ない。
const fn health_reason_label(reason: HealthReason) -> &'static str {
    match reason {
        HealthReason::DaemonUnresponsive => "daemon 無応答",
        HealthReason::MetricsStalled => "metrics 停滞",
        HealthReason::TerminalOutputDropped => "端末出力の欠落",
        HealthReason::TerminalBackpressure => "端末出力の滞留",
        HealthReason::PrScanIncomplete => "PR 検出の欠落",
        HealthReason::MetricsUpdatesDropped => "更新の取りこぼし",
        HealthReason::BackgroundWorkerStopped => "worker 停止",
    }
}

fn load_style(value: u64, busy: u64, hot: u64) -> Style {
    if value >= hot {
        Role::Danger.style()
    } else if value >= busy {
        Role::Warning.style()
    } else {
        // The mascot row is pink. Set white explicitly so a calm metric does
        // not inherit that outer foreground colour before becoming dim.
        Style::new().fg(Color::White).dim()
    }
}

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

/// Convert a frame-cell pointer position into the retained-terminal viewport row
/// and column currently rendered in the right pane, or `None` when the pointer is
/// outside the live content window. `rows_len` and `scroll` describe the same
/// bottom-anchored window [`widgets::live_terminal`] draws, so a drag maps back to
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
    let content_row = body_row.checked_sub(widgets::live_terminal::RIGHT_PANE_CONTENT_TOP)?;
    let body_height = height.saturating_sub(CHROME_ROWS);
    let content_cap = body_height.saturating_sub(
        widgets::live_terminal::RIGHT_PANE_CONTENT_TOP + widgets::live_terminal::FOOTER_ROWS,
    );
    if content_row >= content_cap {
        return None;
    }
    let start = widgets::live_terminal::window_start(rows_len, content_cap, scroll);
    Some(TerminalPoint {
        row: start + content_row,
        column,
    })
}

/// controller が runtime ごとに保持する phase を、Garden の表示語彙へ写す。
/// `Done` は controller が runtime event の `Ended` / `Exited` / `Interrupted` を共通化した
/// 値なので、Garden では静止した完了 pose に写す。
const fn garden_phase(phase: TargetPhase) -> AgentPhase {
    match phase {
        TargetPhase::Absent => AgentPhase::Absent,
        TargetPhase::Ready => AgentPhase::Ready,
        TargetPhase::Running => AgentPhase::Running,
        TargetPhase::Waiting => AgentPhase::Waiting,
        TargetPhase::Done => AgentPhase::Ended,
    }
}

/// Coarse fallback used when a runtime exists in the latest daemon inventory
/// but this TUI has not observed a runtime-local phase push for it yet.
const fn garden_inventory_phase(state: AgentRuntimeInventoryState) -> AgentPhase {
    match state {
        AgentRuntimeInventoryState::Reserved => AgentPhase::Ready,
        AgentRuntimeInventoryState::Live => AgentPhase::Running,
        AgentRuntimeInventoryState::Interrupted | AgentRuntimeInventoryState::Unavailable => {
            AgentPhase::Interrupted
        }
        AgentRuntimeInventoryState::Exited | AgentRuntimeInventoryState::Reclaimed => {
            AgentPhase::Exited
        }
    }
}

/// 壁時計から Garden の animation tick を導く。
///
/// Garden 専用 timer を持たず、frame の素材である `now` だけから決まるので、同じ
/// 時刻の再描画は同じ frame になる。
///
/// 解像度は **1 秒**である。interactive shell は frame material の壁時計を秒へ切り捨てて
/// 再描画を間引くため、それより細かい tick は観測できない（描画されない animation を
/// 定義しても、pose が飛ぶだけで滑らかにはならない）。
fn garden_tick(now: DateTime<Utc>) -> u64 {
    now.timestamp().unsigned_abs() % widgets::garden::ANIMATION_CYCLE_TICKS
}

/// Garden が Home frame を置き換えている frame を、rows と hitbox の両方で返す。
///
/// 描画と hit test の**単一の layout 関数**である。click 解決が座標から session 順を
/// 再計算しないのは、[`render_home_at`] が描いたのと同じ呼び出しが返した
/// `SessionId` 付き rectangle をそのまま使うためで、CJK label・端末 resize・表示上限で
/// click target がずれる余地がない。
///
/// overlay が閉じているか、端末が Garden の最小サイズに満たない場合は `None`（この
/// frame は通常の Home であり、click も通常の Home の hit test に従う）。
fn garden_frame(
    raw_height: usize,
    raw_width: usize,
    home: &HomeProjection,
    now: DateTime<Utc>,
) -> Option<widgets::garden::GardenFrame> {
    let sessions = home.garden_sessions.as_ref()?;
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    widgets::garden::render(
        height,
        width,
        &home.workspace_name,
        sessions,
        garden_tick(now),
        home.garden_motion.is_reduced(),
    )
}

/// Garden を描ける端末サイズか。
///
/// 自動表示の抑止に使う。reducer は overlay と drawer の eligibility だけを持ち、
/// 「何桁・何行あれば庭が描けるか」は renderer の layout 事実なので presentation 側に
/// 置く（高さ 14 行未満・幅 64 桁未満では screen saver で操作可能な一覧を覆わない）。
/// 判定は [`widgets::garden::render`] と同じ正規化サイズで行う。
#[must_use]
pub fn garden_fits(raw_height: usize, raw_width: usize) -> bool {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    height >= widgets::garden::MIN_HEIGHT && width >= widgets::garden::MIN_WIDTH
}

/// Garden 上の click を、frame と同じ layout の hitbox で解決する。
///
/// `None` は「この frame は Garden ではない」で、呼び出し側は通常の Home の hit test
/// を続ける。
#[must_use]
pub fn garden_click_at(
    raw_height: usize,
    raw_width: usize,
    home: &HomeProjection,
    now: DateTime<Utc>,
    column: u16,
    row: u16,
) -> Option<GardenClick> {
    let frame = garden_frame(raw_height, raw_width, home, now)?;
    let (column, row) = (usize::from(column), usize::from(row));
    Some(
        frame
            .hitboxes
            .iter()
            .find(|hitbox| hitbox.contains(column, row))
            .map_or(GardenClick::Dismiss, |hitbox| {
                GardenClick::Visit(hitbox.session_id)
            }),
    )
}

/// controller projection の Home frame を描く。
///
/// 既存 Workspace view と同じ header / 2-pane geometry / viewport を使う。左側の gutter は
/// navigation cursor と command target を stable [`Selection`] / [`Target`] identity から別々に
/// 投影する。Switch では cursor が優先し、Closeup では cursor を抑止して current marker を残す。
#[must_use]
pub fn render_home(raw_height: usize, raw_width: usize, home: &HomeProjection) -> Vec<String> {
    render_home_at(raw_height, raw_width, home, Utc::now())
}

/// [`render_home`] against an explicit wall clock.
///
/// The sidebar's per-session relative timestamps are the only part of the Home
/// frame that depends on the current time. The interactive shell passes the
/// clock in so that time is part of the frame's material and a redraw can be
/// skipped exactly when nothing — the clock included — has changed (#554).
#[must_use]
pub fn render_home_at(
    raw_height: usize,
    raw_width: usize,
    home: &HomeProjection,
    now: DateTime<Utc>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    // Garden は Home の全幅レイヤーなので、収まるなら frame をそのまま置き換える。
    // 収まらない端末では Garden を開かず Home を保つ（操作できる一覧を警告画面で
    // 覆わない）。
    if let Some(frame) = garden_frame(raw_height, raw_width, home, now) {
        return frame.rows;
    }
    let split = panes::split(width, LEFT_WIDTH);
    let body_height = height.saturating_sub(CHROME_ROWS);
    let mut frame = Vec::with_capacity(height);
    frame.push(home_header_line(width, home));
    frame.push(home_notice_banner(width, home));
    let right = dim_inactive_right_pane(
        !home.right_pane_focused(),
        home_right_pane(body_height, split.right, home),
    );
    frame.extend(panes::join(
        body_height,
        &home_left_pane(body_height, split.left, home, now),
        &right,
        split,
    ));
    frame.truncate(height);
    let frame = if let Some(drawer) = &home.director_drawer {
        director_drawer::render_over(height, width, &frame, drawer)
    } else {
        frame
    };
    if let Some(modal) = &home.overview_modal {
        overview_modal::render_over(height, width, &frame, modal)
    } else if home.daemon_overlay {
        daemon_modal::render_over(
            height,
            width,
            &frame,
            daemon_modal::DaemonProjection {
                metrics: home.metrics.as_ref(),
                health: home.health.evaluate(now.timestamp_millis()),
                sessions: home.session_states,
                session_total: home.sessions.len(),
                runtimes: home.daemon_runtimes.as_deref(),
            },
        )
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
        &PrModal::with_selection(overlay.prs().to_vec(), overlay.selected())
            .with_filter(overlay.filter().label()),
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

/// Apply the inactive treatment whenever the right pane does not own input
/// ([`HomeProjection::right_pane_focused`]). Modals are composed after this
/// frame, preserving their foreground styles.
fn dim_inactive_right_pane(inactive: bool, right: Vec<String>) -> Vec<String> {
    if inactive {
        right
            .into_iter()
            // An inactive preview does not own text input. Drop the live PTY's
            // renderer-only cursor marker as well as dimming its visual cell,
            // otherwise the host cursor (and IME candidate window) still says
            // that the terminal has focus in Switch or behind an overlay.
            .map(|line| widgets::dim_ansi(&line.replace(TERMINAL_CURSOR_MARKER, "")))
            .collect()
    } else {
        right
    }
}

fn home_header_line(width: usize, home: &HomeProjection) -> String {
    home_header_layout(width, home).line
}

fn home_notice_banner(width: usize, home: &HomeProjection) -> String {
    let Some(decision) = home
        .decisions
        .iter()
        .find(|item| home.unread_decision_ids.contains(&item.decision_id))
    else {
        return header_spacer(width);
    };
    widgets::clip_to_width(
        &format!(
            "  🔔 {}: {}  (click bell to review)",
            decision
                .owner
                .session_id
                .as_ref()
                .map_or_else(|| "workspace root".to_owned(), ToString::to_string),
            decision.title
        ),
        width,
    )
}

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
    let pr_width = sidebar_pr_width(&home.sessions);
    if height == 1 {
        return home_row_lines_at(width, home, rows[0], columns, pr_width, now)
            .into_iter()
            .take(1)
            .collect();
    }
    let body_capacity = height - 1;
    // Reuse the legacy metric projection so both render paths draw an identical
    // sidecar. An absent observation yields no metrics row, which keeps the
    // pre-metrics home frame byte-for-byte unchanged. The diagnostic health
    // badge is evaluated against the frame's own clock — the renderer stays the
    // only place that reads time, so `now` remains the whole time input.
    let sidecar = sidecar_labels(
        width,
        home.metrics.as_ref(),
        home.health.evaluate(now.timestamp_millis()),
        home.session_states,
    );
    // 明示された mascot speech を優先し、無ければ正常系以外の daemon 状態を吹き出しへ落とす。
    let daemon_speech = abnormal_daemon_speech(home.feedback.as_ref());
    let speech = home.mascot_speech.as_ref().or(daemon_speech.as_ref());
    let mascot =
        widgets::mascot::sidebar_block_with_sidecar(width, home.mascot_tick, speech, &sidecar);
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
    // Reserve the loading skeleton's rows outside the selectable-row budget so a
    // pending create draws just above `+ new session` without scrolling it off.
    let skeleton_rows = if home.create_pending.is_some() {
        CREATE_SKELETON_ROWS
    } else {
        0
    };
    let viewport_capacity = content_capacity.saturating_sub(skeleton_rows);
    let selected_index = rows
        .iter()
        .position(|row| *row == home.selected)
        .unwrap_or(0);
    let start = home_viewport_start(width, home, &rows, selected_index, viewport_capacity);
    let mut lines = Vec::with_capacity(height);
    // Track only selectable-row lines against `viewport_capacity`; the skeleton
    // lives in the space reserved above and never counts toward the break.
    let mut row_line_count = 0;
    for row in &rows[start..] {
        if matches!(row, Selection::NewSession)
            && let Some(name) = home.create_pending.as_deref()
        {
            lines.extend(create_skeleton_lines(width, name, home.mascot_tick));
        }
        let row_lines = home_row_lines_at(width, home, *row, columns, pr_width, now);
        if row_line_count + row_lines.len() > viewport_capacity {
            break;
        }
        row_line_count += row_lines.len();
        lines.extend(row_lines);
    }
    lines.resize(content_capacity, String::new());
    if show_mascot {
        lines.extend(mascot.expect("shown mascot exists").rows().iter().cloned());
        lines.push(String::new());
    }
    let footer = match home.mode {
        HomeMode::Switch => "[switch] ↑↓ select / Enter closeup",
        HomeMode::Closeup => {
            "[closeup] Ctrl-O: x/Ctrl-X close / o switch / a/Ctrl-A actions / n/p tabs"
        }
    };
    lines.push(
        Style::new()
            .dim()
            .paint(&widgets::clip_to_width(footer, width)),
    );
    lines
}

fn home_viewport_start(
    width: usize,
    home: &HomeProjection,
    rows: &[Selection],
    selected: usize,
    capacity: usize,
) -> usize {
    let mut start = 0;
    while start < selected
        && rows[start..=selected]
            .iter()
            .map(|row| home_row_height_at(width, home, *row))
            .sum::<usize>()
            > capacity
    {
        start += 1;
    }
    start
}

/// Rows the create loading skeleton occupies, matching a session row's height so
/// the daemon's landed `session.created` row replaces it without shifting the
/// sidebar.
const CREATE_SKELETON_ROWS: usize = 2;

/// Two-line loading skeleton for a create the daemon is still fulfilling, drawn
/// just above `+ new session` (`document/03-tui.md`). The activity glyph and the
/// typed name share one slow left-to-right wave — the same [`widgets::Shimmer`]
/// sweep the removal skeleton and pending tabs use — so a pending create reads as
/// loading, not a static row. `tick` is the shared sidebar mascot frame, so the
/// wave advances only on `AppEvent::Tick`. New-session feedback uses Success
/// (green), never Accent (cyan), and is never a cursor or current target.
fn create_skeleton_lines(width: usize, name: &str, tick: u64) -> Vec<String> {
    let wave = widgets::Shimmer {
        style: Role::Success.style().bold(),
        base_style: Role::Success.style().dim(),
        speed: 4,
    };
    let frame = usize::try_from(tick).unwrap_or(usize::MAX);
    vec![
        widgets::pad_to_width(
            &format!(
                "  {}",
                widgets::shimmer_text_with(&format!("+ {name}"), frame, wave)
            ),
            width,
        ),
        widgets::pad_to_width(
            &format!("  {}", widgets::shimmer_text_with("creating…", frame, wave)),
            width,
        ),
    ]
}

fn home_row_height(row: Selection) -> usize {
    usize::from(matches!(row, Selection::Target(Target::Session(_)))) + 1
}

/// Row height accounting for the inline create form. When the `+ new session` row
/// owns input its height is the number of lines `new_session_input_lines` draws
/// (caret row plus any wrapped error), so the viewport reserves exactly what is
/// rendered. Every other row falls back to the static [`home_row_height`].
fn home_row_height_at(width: usize, home: &HomeProjection, row: Selection) -> usize {
    if let (Selection::NewSession, Some(draft)) = (row, home.create_draft.as_ref()) {
        return create_session_input_lines(width, draft, home.create_role.as_deref()).len();
    }
    home_row_height(row)
}

/// Paint a Home sidebar row label with v1's colour precedence.
///
/// `+ new session` is a Success (green) affordance in every mode (#302 / #362):
/// resolve it before the generic accent branches so the Switch cursor only adds
/// bold and it never falls through to the accent (cyan) `selected` colour that
/// real targets use. A non-cursor `+ new session` in Switch still takes the
/// shared inactive dim, and Closeup keeps it Success but unbolded. Every other
/// row keeps the established order: selected cursor (accent bold) → Switch
/// inactive dim → current (accent bold) → plain accent.
fn home_row_label(
    row: Selection,
    label: &str,
    selected: bool,
    current: bool,
    mode: HomeMode,
) -> String {
    if matches!(row, Selection::NewSession) {
        return if selected {
            Role::Success.style().bold().paint(label)
        } else if mode == HomeMode::Switch {
            Style::new().dim().paint(label)
        } else {
            Role::Success.style().paint(label)
        };
    }
    if selected {
        Role::Accent.style().bold().paint(label)
    } else if mode == HomeMode::Switch {
        Style::new().dim().paint(label)
    } else if current {
        Role::Accent.style().bold().paint(label)
    } else {
        Role::Accent.style().paint(label)
    }
}

/// Render a `Failed` session row: a danger-toned label tagged `failed` with its
/// safe failure reason below. The row is not a usable checkout (attach is gated
/// in the controller by `can_use=false`) but stays removable (`can_remove=true`).
fn home_failed_row_lines(
    session: &ProjectedSession,
    row: Selection,
    width: usize,
    selected: bool,
    current: bool,
) -> Vec<String> {
    let clipped = widgets::clip_to_width(&session.label, width.saturating_sub(9));
    let label = if selected {
        Role::Danger.style().bold().paint(&clipped)
    } else {
        Role::Danger.style().dim().paint(&clipped)
    };
    let marker = home_row_marker(row, selected, current);
    if session.failure_stage == Some(FailureStage::Delete) {
        let first = widgets::pad_to_width(&format!("{marker} {label}"), width);
        let detail = format!(
            "{} remove failed",
            home_session_continuation_marker(selected, current)
        );
        return vec![
            first,
            widgets::pad_to_width(&Style::new().dim().paint(&detail), width),
        ];
    }
    let tag = Role::Danger.style().dim().paint("failed");
    let first = widgets::pad_to_width(&format!("{marker} {label}  {tag}"), width);
    let reason = session
        .failure_summary
        .as_deref()
        .unwrap_or("session create failed");
    let reason = format!(
        "{} {reason}",
        home_session_continuation_marker(selected, current)
    );
    let reason = Style::new()
        .dim()
        .paint(&widgets::clip_to_width(&reason, width));
    vec![first, widgets::pad_to_width(&reason, width)]
}

#[allow(clippy::too_many_lines)] // One total row renderer keeps lifecycle, badge, and viewport height composition aligned.
fn home_row_lines_at(
    width: usize,
    home: &HomeProjection,
    row: Selection,
    columns: SidebarDiffColumns,
    pr_width: usize,
    now: DateTime<Utc>,
) -> Vec<String> {
    // When the create form owns input, the `+ new session` row becomes an inline
    // name-only caret in place of its static label, with any validation error
    // wrapped to the sidebar width below it. The row height therefore varies with
    // the error, which `home_row_height_at` derives from the same builder so the
    // viewport math stays aligned.
    if let (Selection::NewSession, Some(draft)) = (row, home.create_draft.as_ref()) {
        return create_session_input_lines(width, draft, home.create_role.as_deref());
    }
    let target = match row {
        Selection::Target(target) => Some(target),
        Selection::NewSession => None,
    };
    let (label, detail, session) = match row {
        // Root is not part of managed Home rows. Keep the branch total for
        // stale synthetic projections without rendering a hidden root action.
        Selection::Target(Target::Root(_)) => ("", "", None),
        Selection::Target(Target::Session(id)) => home
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map_or(("", "", None), |session| {
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
    let current = target.and_then(Target::session_id) == home.active;
    // The green rail identifies the Closeup command target. Switch already has
    // an explicit cursor, so retaining the previous target's rail there creates
    // two competing selections.
    let current_marker = current && home.mode == HomeMode::Closeup;
    if let Some(session) = session.filter(|session| session.lifecycle == SessionLifecycle::Failed) {
        return home_failed_row_lines(session, row, width, selected, current_marker);
    }
    let marker = home_row_marker(row, selected, current_marker);
    let label = if session.is_some() {
        widgets::clip_to_width(label, width.saturating_sub(6))
    } else {
        label.to_string()
    };
    let label = home_row_label(row, &label, selected, current, home.mode);
    let first = if let Some(session) = session {
        let note = if session.has_notes { "✎" } else { "·" };
        let badge = session
            .role_id
            .as_ref()
            .map(|role| format!(" [{}]", widgets::clip_to_width(role, 12)))
            .unwrap_or_default();
        widgets::pad_to_width(
            &format!(
                "{marker} {label}{badge}  {}",
                Style::new().dim().paint(note)
            ),
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
        let mut facts = Vec::new();
        if let Some(resume) = session.agent_resume.and_then(resume_label) {
            facts.push(resume.to_owned());
        }
        facts.push(modified);
        let metadata = format!(
            "{} {}",
            home_session_continuation_marker(selected, current_marker),
            facts.join(" · ")
        );
        // Draw the same Git summary columns as the legacy sidebar. A non-cursor
        // Switch row is inactive even when its Git cells contain ANSI spans:
        // compose its dim treatment after the spans so
        // their resets cannot make the relative time bright. The cursor row
        // keeps its established marker emphasis. Column widths still reuse the
        // shared `sidebar_metadata` so both render paths align identically.
        let inactive = home.mode == HomeMode::Switch && !selected;
        // A selected Switch row keeps its descriptive prefix subdued while its
        // Git cells remain at normal intensity. Closeup owns input, so its
        // entire metadata row stays at normal intensity. Inactive Switch rows
        // are dimmed as a whole below.
        let metadata = if home.mode == HomeMode::Switch && !inactive {
            Style::new().dim().paint(&metadata)
        } else {
            metadata
        };
        let metadata = sidebar_metadata(
            &metadata,
            home.git_diffs.get(&session.id),
            columns,
            session.pr_summary.as_deref(),
            pr_width,
            width,
            inactive,
        );
        let metadata = if inactive {
            widgets::dim_ansi(&metadata)
        } else {
            metadata
        };
        vec![first, widgets::pad_to_width(&metadata, width)]
    } else {
        vec![first]
    }
}

fn resume_label(projection: ProviderResumeProjection) -> Option<&'static str> {
    if !projection.interrupted {
        return None;
    }
    Some(if projection.resumable {
        "interrupted · resume available"
    } else {
        match projection.reason {
            ProviderResumeReason::ProviderMetadataUnavailable
            | ProviderResumeReason::ExplicitResumeAvailable => "interrupted · resume unavailable",
            ProviderResumeReason::AmbiguousProviderMetadata => {
                "interrupted · resume metadata ambiguous"
            }
            ProviderResumeReason::IncompatibleProviderMetadata => {
                "interrupted · resume metadata incompatible"
            }
            ProviderResumeReason::LiveOrOwnershipUnknown => {
                "interrupted · resume ownership unknown"
            }
            ProviderResumeReason::SourceAlreadySuperseded => "interrupted · already resumed",
        }
    })
}

/// Build the inline `+ new session` sidebar lines: the caret row, then any
/// validation error wrapped to the sidebar `width` below it.
///
/// The caret row shows the `+ new:` affordance in the Success (green) role so it
/// stays visually continuous with the static green `+ new session` label,
/// followed by a white block-caret input (`+ new: <name>`, documented in
/// 03-tui.md). No selection `>` chevron is drawn:
/// the row already owns input, so the chevron is redundant here; a blank marker
/// column keeps the affordance aligned with the static label. It stays a single
/// clipped line since the name is the user's own bounded input. Any reducer
/// validation message is wrapped **as plain text** to the display width and each
/// segment is then painted in the danger style — wrapping the styled string
/// directly would miscount ANSI escapes as visible columns. Every line is padded
/// to `width` so CJK, styled, and narrow-width rows keep their columns aligned and
/// nothing is silently truncated. profile / model never appear here.
///
/// The returned line count is authoritative: `home_row_height_at` reports the same
/// length so the viewport's scroll math never diverges from what is drawn.
fn new_session_input_lines(width: usize, draft: &CreateDraft) -> Vec<String> {
    // New-session input deliberately uses the neutral white foreground rather
    // than the generic Accent (cyan) editing role. No `>` selection chevron is
    // drawn (the row owns input, so it is redundant), but a blank marker column
    // keeps the affordance aligned with the static label.
    let input = Style::new().fg(Color::White).bold();
    let affordance = Role::Success.style().bold();
    let caret = widgets::block_caret(&draft.name, draft.name.chars().count(), &input);
    let caret_line = format!("  {} {caret}", affordance.paint("+ new:"));
    let mut lines = vec![widgets::pad_to_width(
        &widgets::clip_to_width(&caret_line, width),
        width,
    )];
    if let Some(error) = &draft.error {
        let danger = Role::Danger.style().bold();
        for segment in widgets::wrap_to_width(error, width) {
            lines.push(widgets::pad_to_width(&danger.paint(&segment), width));
        }
    }
    lines
}

fn create_session_input_lines(
    width: usize,
    draft: &CreateDraft,
    role: Option<&str>,
) -> Vec<String> {
    let mut lines = new_session_input_lines(width, draft);
    if let Some(role) = role {
        let role_line = format!("  role: {role}  (↑/↓)");
        lines.insert(
            1,
            widgets::pad_to_width(
                &Style::new()
                    .dim()
                    .paint(&widgets::clip_to_width(&role_line, width)),
                width,
            ),
        );
    }
    lines
}

/// v1-compatible sidebar marker with explicit precedence.
///
/// A selected session starts with v1's usagi glyph and uses a red `|` continuation;
/// in Closeup its active two-line stack is green. Switch does not retain a rail
/// for the previous target because its cursor is the sole selection indicator.
/// The action row remains chevron-free even while it owns the Switch cursor.
fn home_row_marker(row: Selection, selected: bool, current: bool) -> String {
    if selected {
        return match row {
            Selection::Target(Target::Session(_)) => Role::Danger.style().bold().paint("\u{f0907}"),
            Selection::Target(Target::Root(_)) | Selection::NewSession => " ".to_string(),
        };
    }
    if current {
        return Role::Success.style().bold().paint("|");
    }
    " ".to_string()
}

/// The second row of a session carries the same coloured rail as its identity row.
fn home_session_continuation_marker(selected: bool, current: bool) -> String {
    if selected {
        Role::Danger.style().bold().paint("|")
    } else if current {
        Role::Success.style().bold().paint("|")
    } else {
        " ".to_string()
    }
}

fn home_right_pane(height: usize, width: usize, home: &HomeProjection) -> Vec<String> {
    let mode = match home.mode {
        HomeMode::Switch => "Switch",
        HomeMode::Closeup => "Closeup",
    };
    let header = format!(
        " {}",
        Role::Accent.style().bold().paint(home.preview_label())
    );
    // Switch's pane is a read-only preview of the hovered row, so it says so:
    // the pane the user is looking at is not yet the one commands act on.
    let footer_hint = match home.mode {
        HomeMode::Switch => format!("[{mode}] preview pane"),
        HomeMode::Closeup => format!("[{mode}] active pane"),
    };
    let footer = Style::new()
        .dim()
        .paint(&widgets::clip_to_width(&footer_hint, width));
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
        rows.extend(widgets::live_terminal::render(
            view,
            width,
            height.saturating_sub(rows.len()),
            height.saturating_sub(
                widgets::live_terminal::RIGHT_PANE_CONTENT_TOP
                    + widgets::live_terminal::FOOTER_ROWS,
            ),
            &footer_hint,
        ));
        return rows;
    }
    with_footer_gap(
        vec![
            chrome[0].clone(),
            chrome[1].clone(),
            String::new(),
            Style::new().dim().paint(&widgets::pad_to_width(
                &home.pane_detail.as_ref().map_or_else(
                    || format!("  agent: {}", phase_label(home.preview_phase)),
                    |detail| format!("  agent: {detail}"),
                ),
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

/// The selected interrupted tab's body line. It states what an explicit Resume
/// does, or why the conversation cannot be resumed, using only the closed
/// display vocabulary of [`InterruptedTab`].
fn interrupted_detail(pane: &crate::usecase::application::pane::InterruptedPane) -> String {
    match pane.resuming {
        Some(_) => "resuming this conversation".to_owned(),
        // The body is one line, so the resumable case states the action and
        // leaves the longer explanation to the unresumable reasons.
        None if pane.tab.resumable() => "interrupted — Ctrl-O r resumes it".to_owned(),
        None => pane.tab.safe_detail().to_owned(),
    }
}

fn phase_label(phase: TargetPhase) -> &'static str {
    match phase {
        TargetPhase::Absent => "absent",
        TargetPhase::Ready => "ready",
        TargetPhase::Running => "running",
        TargetPhase::Waiting => "waiting",
        TargetPhase::Done => "done",
    }
}

/// 正常系以外の daemon 状態を、左 sidebar のうさぎ吹き出しに出す message へ写す。
///
/// 健全に接続され待機・進行しているだけの状態（`None` / `Progress` / `Reconnected`）は
/// 正常系として `None` を返し、うさぎを無言に保つ。切断・再同期要求・操作/端末エラーだけを
/// 注意色（bubble の黄色太字）付きの短い 2 行 message にする。footer の詳細 feedback とは別に、
/// bottom-left のうさぎで一目で異常に気づけるようにするための投影である。
fn abnormal_daemon_speech(feedback: Option<&Feedback>) -> Option<widgets::mascot::MascotSpeech> {
    let lines = match feedback? {
        Feedback::Disconnected => vec!["⚠ daemon 切断".to_owned(), "再接続を待機中".to_owned()],
        Feedback::ResyncRequired => {
            vec!["⚠ 再同期が必要".to_owned(), "状態を同期中".to_owned()]
        }
        Feedback::OperationError(error) => {
            vec!["⚠ 操作エラー".to_owned(), error.message.as_str().to_owned()]
        }
        Feedback::TerminalError(error) => {
            vec!["⚠ 端末エラー".to_owned(), error.message.as_str().to_owned()]
        }
        // 正常系: 待機・進行中・再接続完了はうさぎを無言に保つ。
        Feedback::Progress(_) | Feedback::Reconnected => return None,
    };
    widgets::mascot::MascotSpeech::new(lines)
}

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
        AGENT_ICON, AgentConcurrency, CHROME_ROWS, CPU_ICON, CREATE_SKELETON_ROWS, CreateDraft,
        DaemonMetrics, GIBIBYTE, GitDiff, HEALTH_GLYPH, HomeHeaderAction, HomeProjection,
        LEFT_WIDTH, MEBIBYTE, PR_ICON, PR_RESERVE_WIDTH, ProjectedSession, SIDECAR_GUTTER,
        SidebarDiffColumns, TerminalViewProjection, Workspace, abnormal_daemon_speech,
        create_skeleton_lines, feedback_label, format_memory, garden_click_at, garden_fits,
        garden_frame, garden_tick, health_badge, health_reason_label, home_header_action_at,
        home_header_layout, home_left_pane, home_row_lines_at, home_viewport_start, load_style,
        new_session_input_lines, pane_tab_label, pane_tab_selected, phase_label, render_home,
        render_home_at, resume_label, short_id, sidebar_metadata, sidecar_labels,
        terminal_point_at, with_footer_gap,
    };
    use crate::presentation::theme::{Color, Role, Style};
    use crate::presentation::views::director_drawer::{
        DIRECTOR_ICON, DirectorConversation, DirectorDrawerProjection, DirectorNewProjection,
    };
    use crate::presentation::widgets::mascot::MascotSpeech;
    use crate::presentation::widgets::{self, display_width, modal, wrap_to_width};
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, Feedback, GardenClick, HomeMode, RoleChoice,
        Route, SafeError, SafeMessage, Selection, SessionRoleCatalog, SessionRoleProjection,
        Target, TargetPhase, update,
    };
    use crate::usecase::application::pane::{
        PaneEvent, PaneKind, PaneSelection, PaneState, PaneTab, TabSelection, reduce,
    };
    use crate::usecase::application::terminal_selection::TerminalPoint;
    use std::path::Path;

    use chrono::{DateTime, Utc};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use usagi_core::domain::agent::{
        AgentInventory, AgentRuntimeInventoryItem, AgentRuntimeInventoryState,
        ProviderResumeProjection, ProviderResumeReason,
    };
    use usagi_core::domain::id::{
        AgentContinuationRef, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, OperationId,
        SessionId, TerminalId, TerminalRef, UserDecisionId, WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::pullrequest::{PrLink, PrState};
    use usagi_core::domain::role::RoleId;
    use usagi_core::domain::session_lifecycle::{AgentPhase, FailureStage, SessionLifecycle};

    use usagi_core::domain::session::{SessionOrigin, SessionRecord};

    use usagi_core::domain::workspace::Workspace as WorkspaceRecord;
    use usagi_core::domain::workspace_state::WorkspaceState;
    use usagi_core::usecase::daemon_health::{DaemonHealth, DaemonHealthTracker, HealthReason};
    use usagi_core::usecase::session_state::SessionStateCounts;

    #[test]
    fn ordered_frame_projection_reuses_owned_session_git_and_terminal_components() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let record = SessionRecord {
            name: "alpha".to_owned(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: PathBuf::from("/work/alpha"),
            created_at: Utc::now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        };
        let sessions: Arc<[ProjectedSession]> =
            Arc::from([ProjectedSession::from_record(session, &record)]);
        let git = Arc::new(BTreeMap::from([(
            session,
            GitDiff {
                base: "main".to_owned(),
                ahead: 1,
                behind: 0,
                added: 2,
                removed: 0,
            },
        )]));
        let terminal = Arc::new(TerminalViewProjection {
            rows: vec!["https://example.com".to_owned()],
            row_offset: 0,
            total_rows: 1,
            scroll: 0,
            feedback: None,
        });

        let projection = HomeProjection::from_ordered_state(&state, "work", Arc::clone(&sessions))
            .with_shared_git_diffs(Arc::clone(&git))
            .with_shared_terminal_view(Some(Arc::clone(&terminal)));

        assert!(Arc::ptr_eq(&projection.sessions, &sessions));
        assert!(Arc::ptr_eq(&projection.git_diffs, &git));
        assert!(Arc::ptr_eq(
            projection.terminal_view.as_ref().unwrap(),
            &terminal
        ));
    }
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
            agent_resume: None,
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
            failure_stage: None,
            failure_summary: None,
            role_id: None,
        }
    }

    fn runtime_ref(workspace: WorkspaceId, session: SessionId) -> AgentRuntimeRef {
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: WorktreeId::new(),
            },
            Some(session),
        )
        .expect("a session owns its own terminal")
    }

    /// Build a Home projection whose rows carry the given daemon-authoritative
    /// lifecycle and, when present, one Agent runtime reporting that phase.
    fn home_with_session_states(rows: &[(SessionLifecycle, Option<AgentPhase>)]) -> HomeProjection {
        let workspace = WorkspaceId::new();
        let ids = rows.iter().map(|_| SessionId::new()).collect::<Vec<_>>();
        let mut state = AppState::home(workspace, ids.clone());
        let mut projected = Vec::new();
        for (index, (lifecycle, phase)) in rows.iter().enumerate() {
            let id = ids[index];
            if let Some(phase) = *phase {
                let _ = update(
                    &mut state,
                    AppEvent::Backend(BackendEvent::RuntimePhase {
                        runtime: runtime_ref(workspace, id),
                        phase,
                    }),
                );
            }
            let mut session = projected_session(id, &format!("s{index}"), "/work");
            session.lifecycle = *lifecycle;
            projected.push(session);
        }
        HomeProjection::from_state(&state, "atlas", Path::new("/work"), &projected)
    }

    fn daemon_metrics() -> DaemonMetrics {
        DaemonMetrics {
            schema_version: 3,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * MEBIBYTE,
            active_subscribers: 1,
            dropped_updates: 0,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: Some(AgentConcurrency {
                in_use: 1,
                limit: 16,
            }),
            failed_background_workers: 0,
        }
    }

    /// The summary counts each session exactly once under the documented
    /// precedence, and never reports a finished or interrupted runtime as a
    /// failure — only a `Failed` lifecycle is a failure.
    #[test]
    fn home_sidebar_summarises_running_waiting_and_failed_sessions() {
        let home = home_with_session_states(&[
            (SessionLifecycle::Available, Some(AgentPhase::Running)),
            (SessionLifecycle::Available, Some(AgentPhase::Running)),
            (SessionLifecycle::Available, Some(AgentPhase::Waiting)),
            (SessionLifecycle::Failed, None),
            (SessionLifecycle::Available, Some(AgentPhase::Interrupted)),
            (SessionLifecycle::Available, Some(AgentPhase::Exited)),
            (SessionLifecycle::Available, Some(AgentPhase::Ended)),
            (SessionLifecycle::Initializing, None),
        ]);

        let frame = joined_home(&home);
        assert!(frame.contains("run 2 wait 1 fail 1"), "{frame}");
    }

    /// A failed row and a still-live phase report can overlap for a frame while
    /// the snapshot catches up. `Failed` wins, so the session is not counted twice.
    #[test]
    fn a_failed_session_outranks_a_runtime_that_still_reports_running() {
        let home =
            home_with_session_states(&[(SessionLifecycle::Failed, Some(AgentPhase::Running))]);
        let frame = joined_home(&home);
        assert!(frame.contains("fail 1"), "{frame}");
        assert!(!frame.contains("run "), "{frame}");
    }

    #[test]
    fn the_session_state_summary_omits_the_classes_with_no_session() {
        let home =
            home_with_session_states(&[(SessionLifecycle::Available, Some(AgentPhase::Waiting))]);
        let frame = joined_home(&home);
        assert!(frame.contains("wait 1"), "{frame}");
        assert!(!frame.contains("run "), "{frame}");
        assert!(!frame.contains("fail "), "{frame}");
    }

    /// An empty workspace and a workspace with nothing to report both leave the
    /// sidecar as it was before the summary existed, rather than drawing zeros.
    #[test]
    fn a_quiet_workspace_draws_no_session_state_row() {
        let baseline = joined_home(&home_with_session_states(&[]));
        let quiet = joined_home(&home_with_session_states(&[
            (SessionLifecycle::Available, Some(AgentPhase::Ready)),
            (SessionLifecycle::Deleting, None),
        ]));
        for frame in [&baseline, &quiet] {
            for label in ["run ", "wait ", "fail "] {
                assert!(!frame.contains(label), "{label} in {frame}");
            }
        }
    }

    /// The counts are derived from the lifecycle / phase projection, not from the
    /// daemon metrics schema, so they are drawn even while no observation has
    /// arrived — and they never displace the metrics row.
    #[test]
    fn the_state_row_needs_no_metrics_and_sits_above_the_metrics_row() {
        let home =
            home_with_session_states(&[(SessionLifecycle::Available, Some(AgentPhase::Running))]);

        let without = home_left_pane(30, LEFT_WIDTH, &home, now());
        assert!(without.iter().any(|line| strip(line).contains("run 1")));
        assert!(without.iter().all(|line| !strip(line).contains(CPU_ICON)));

        let with_metrics = home_left_pane(
            30,
            LEFT_WIDTH,
            &home.clone().with_metrics(Some(daemon_metrics())),
            now(),
        );
        let state_row = with_metrics
            .iter()
            .position(|line| strip(line).contains("run 1"))
            .expect("session state row");
        let concurrency_row = with_metrics
            .iter()
            .position(|line| strip(line).contains(AGENT_ICON))
            .expect("agent concurrency row");
        let metrics_row = with_metrics
            .iter()
            .position(|line| strip(line).contains(CPU_ICON))
            .expect("daemon metrics row");
        // The state row stays topmost and the process row stays bottom-most; the
        // Agent concurrency projection occupies the row between them.
        assert_eq!(concurrency_row, state_row + 1);
        assert_eq!(metrics_row, state_row + 2);
        // Every status begins in the same column beside the rabbit.
        assert_eq!(
            strip(&with_metrics[state_row]).find("run 1"),
            strip(&with_metrics[metrics_row]).find(CPU_ICON)
        );
        assert_eq!(
            strip(&with_metrics[state_row]).find("run 1"),
            strip(&with_metrics[concurrency_row]).find(AGENT_ICON)
        );
    }

    /// Narrow sidebars keep the existing degradation: too narrow for the rabbit
    /// drops the whole mascot block, and a middling width clips the status text
    /// instead of overflowing the pane.
    #[test]
    fn a_narrow_sidebar_drops_or_clips_the_state_row_without_overflowing() {
        let home = home_with_session_states(&[(SessionLifecycle::Failed, None)]);

        let dropped = home_left_pane(20, 8, &home, now());
        assert!(dropped.iter().all(|line| !strip(line).contains("fail 1")));
        assert!(dropped.iter().all(|line| display_width(line) <= 8));

        let clipped = home_left_pane(20, 14, &home, now());
        assert!(clipped.iter().all(|line| display_width(line) <= 14));
    }

    fn joined_home(home: &HomeProjection) -> String {
        render_home(30, 100, home)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn home_header_director_button_is_dim_until_its_drawer_is_frontmost() {
        // The accent in the right-hand strip marks what is in front. A closed
        // drawer must read like an unselected mode chip (dim) in both Home
        // modes, so it cannot compete with the active mode for attention.
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let dim_button = Style::new()
            .dim()
            .paint(&format!("[ {DIRECTOR_ICON} director ]"));
        let accent_button = Role::Accent
            .style()
            .bold()
            .reverse()
            .paint(&format!("[ {DIRECTOR_ICON} director ]"));

        let mut closed = HomeProjection::from_state(&state, "atlas", Path::new("/work"), &[]);
        assert!(home_header_layout(80, &closed).line.ends_with(&dim_button));
        closed.mode = HomeMode::Closeup;
        assert!(home_header_layout(80, &closed).line.ends_with(&dim_button));

        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
        let mut open = HomeProjection::from_state(&state, "atlas", Path::new("/work"), &[]);
        assert!(home_header_layout(80, &open).line.ends_with(&accent_button));
        open.mode = HomeMode::Closeup;
        assert!(home_header_layout(80, &open).line.ends_with(&accent_button));

        // The clipped narrow-width fallback drops the mode toggle but keeps the
        // same closed-state dimming, so the button never brightens as it shrinks.
        let clipped = home_header_layout(10, &closed).line;
        assert!(clipped.starts_with("\u{1b}[2m"));
        assert!(!clipped.contains("36"));
    }

    #[test]
    fn home_header_layout_and_hit_test_share_cjk_notice_and_drawer_geometry() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        // A pending decision makes the notice badge unread; closing its
        // auto-opened overlay leaves the badge on the header without an overlay.
        let decision = usagi_core::domain::user_decision::UserDecision {
            decision_id: UserDecisionId::new(),
            owner: usagi_core::domain::user_decision::UserDecisionOwner {
                workspace_id: workspace,
                session_id: None,
                caller: usagi_core::domain::agent::CallerRef {
                    session_id: None,
                    agent_id: usagi_core::domain::id::AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "confirm".to_owned(),
            prompt: String::new(),
            options: vec![usagi_core::domain::user_decision::UserDecisionOption {
                id: "ok".to_owned(),
                label: "ok".to_owned(),
                description: None,
            }],
            allow_freeform: false,
            expires_at: None,
            idempotency_key: None,
            status: usagi_core::domain::user_decision::UserDecisionStatus::Pending,
            answer: None,
            created_at: now(),
            resolved_at: None,
        };
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![decision],
            }),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        assert_eq!(state.overlay(), None);
        let mut home =
            HomeProjection::from_state(&state, "日本語 workspace", Path::new("/work"), &[]);

        let layout = home_header_layout(100, &home);
        assert_eq!(display_width(&layout.line), 100);
        assert!(strip(&layout.line).contains(&format!("{DIRECTOR_ICON} director")));
        assert!(strip(&layout.line).contains("notice"));
        let workspace_columns = (0..100)
            .filter(|column| layout.action_at(*column) == Some(HomeHeaderAction::Director))
            .collect::<Vec<_>>();
        let notice_columns = (0..100)
            .filter(|column| layout.action_at(*column) == Some(HomeHeaderAction::Decisions))
            .collect::<Vec<_>>();
        assert!(!workspace_columns.is_empty());
        assert!(!notice_columns.is_empty());
        for column in workspace_columns {
            assert_eq!(
                home_header_action_at(100, &home, u16::try_from(column).unwrap(), 0),
                Some(HomeHeaderAction::Director)
            );
        }
        for column in notice_columns {
            assert_eq!(
                home_header_action_at(100, &home, u16::try_from(column).unwrap(), 0),
                Some(HomeHeaderAction::Decisions)
            );
        }
        assert_eq!(home_header_action_at(100, &home, 99, 1), None);
        let closed_line = home_header_layout(100, &home).line;

        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
        home = HomeProjection::from_state(&state, "日本語 workspace", Path::new("/work"), &[]);
        // The open drawer highlights the header button (adds the reverse SGR
        // attribute), so its rendered header differs from the closed one.
        let open_line = home_header_layout(100, &home).line;
        assert_ne!(open_line, closed_line);
        assert!(open_line.contains("1;7"));
    }

    #[test]
    fn home_header_narrow_width_clips_button_and_never_exposes_phantom_hits() {
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let home =
            HomeProjection::from_state(&state, "非常に長い workspace 名", Path::new("/work"), &[]);
        for width in [0usize, 1, 8, 18, 56, 80] {
            let normalized = if width == 0 { 80 } else { width };
            let layout = home_header_layout(normalized, &home);
            assert_eq!(display_width(&layout.line), normalized);
            for column in 0..normalized {
                assert_eq!(
                    home_header_action_at(normalized, &home, u16::try_from(column).unwrap(), 0),
                    layout.action_at(column)
                );
            }
            assert_eq!(
                home_header_action_at(normalized, &home, u16::try_from(normalized).unwrap(), 0),
                None
            );
        }
    }

    #[test]
    fn drawer_projection_seam_only_replaces_material_while_the_drawer_is_open() {
        let workspace = WorkspaceId::new();
        let material = DirectorDrawerProjection {
            conversations: vec![DirectorConversation {
                label: "root conversation".to_owned(),
                selected: true,
            }],
            terminal_view: Some(TerminalViewProjection {
                rows: vec!["director agent output".to_owned()],
                row_offset: 0,
                total_rows: 1,
                scroll: 0,
                feedback: None,
            }),
            interrupted_detail: None,
            feedback: None,
            new: DirectorNewProjection::default(),
        };

        let closed_state = AppState::home(workspace, Vec::new());
        let closed = HomeProjection::from_state(&closed_state, "atlas", Path::new("/work"), &[])
            .with_director_drawer(material.clone());
        let closed_text = render_home(20, 100, &closed).join("\n");
        assert!(!closed_text.contains("root conversation"));
        assert!(!closed_text.contains("director agent output"));

        let mut open_state = AppState::home(workspace, Vec::new());
        let _ = update(&mut open_state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
        let open = HomeProjection::from_state(&open_state, "atlas", Path::new("/work"), &[])
            .with_director_drawer(material);
        let open_text = render_home(20, 100, &open).join("\n");
        assert!(open_text.contains("root conversation"));
        assert!(open_text.contains("director agent output"));
    }

    #[test]
    fn create_skeleton_draws_two_padded_lines_that_wave_with_the_tick() {
        let first = create_skeleton_lines(30, "atlas", 0);
        assert_eq!(first.len(), CREATE_SKELETON_ROWS);
        // Both lines are padded to the sidebar width and carry the typed name /
        // the loading caption, so the skeleton reads as a session-height row.
        assert!(first.iter().all(|line| display_width(line) == 30));
        assert!(strip(&first[0]).contains("atlas"));
        assert!(strip(&first[1]).contains("creating"));
        // The sweep is animated, not a static blink: a later tick paints a
        // different frame while keeping the same display width and text.
        let later = create_skeleton_lines(30, "atlas", 12);
        assert_ne!(first[0], later[0]);
        assert!(strip(&later[0]).contains("atlas"));
        assert_eq!(display_width(&later[0]), 30);
    }

    #[test]
    fn home_pending_create_waves_a_skeleton_above_new_session() {
        let workspace = WorkspaceId::new();
        let state = AppState::home(workspace, Vec::new());
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[])
            .with_create_pending(Some("atlas".to_owned()));
        let lines = render_home(30, 100, &home)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>();

        let skeleton = lines
            .iter()
            .position(|line| line.contains("atlas"))
            .expect("pending create name is drawn as a skeleton row");
        let new_session = lines
            .iter()
            .position(|line| line.contains("+ new session"))
            .expect("the new-session affordance is still drawn");
        // The skeleton sits just above `+ new session` and never replaces it.
        assert!(skeleton < new_session);
        assert!(lines[skeleton].contains("creating") || lines[skeleton + 1].contains("creating"));

        // Absent a pending create, no skeleton or loading caption is drawn.
        let quiet = render_home(
            30,
            100,
            &HomeProjection::from_state(&state, "work", Path::new("/work"), &[]),
        )
        .iter()
        .map(|line| strip(line))
        .collect::<Vec<_>>()
        .join("\n");
        assert!(!quiet.contains("atlas"));
        assert!(!quiet.contains("creating"));
    }

    #[test]
    fn interrupted_provider_resume_status_is_visible_without_exposing_an_id() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let mut projected = projected_session(session, "agent-work", "/work/agent");
        projected.agent_resume = Some(ProviderResumeProjection {
            interrupted: true,
            resumable: true,
            reason: ProviderResumeReason::ExplicitResumeAvailable,
        });
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[projected]);

        let frame = joined_home(&home);
        assert!(frame.contains("interrupted · resume available"));
        assert!(!frame.contains("provider-session-id"));
    }

    #[test]
    fn failed_session_row_shows_the_failed_state_and_its_failure_reason() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let mut failed = projected_session(session, "stale", "/work/stale");
        failed.lifecycle = usagi_core::domain::session_lifecycle::SessionLifecycle::Failed;
        // Keep the reason short enough to survive the fixed sidebar-width clip so
        // the test asserts the reason is rendered, not the exact wrap width.
        failed.failure_summary = Some("branch exists".to_owned());
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            std::slice::from_ref(&failed),
        );

        let frame = joined_home(&home);
        // The row is tagged as failed and shows its safe failure reason so the
        // operator can see why the name is stuck and remove it.
        assert!(frame.contains("stale"));
        assert!(frame.contains("failed"));
        assert!(frame.contains("branch exists"));

        // Selecting the failed row keeps the failed treatment (the emphasised
        // cursor variant of the danger label).
        let mut selected_state = AppState::home(workspace, vec![session]);
        let _ = update(&mut selected_state, AppEvent::Key(AppKey::Down));
        let selected = joined_home(&HomeProjection::from_state(
            &selected_state,
            "work",
            Path::new("/work"),
            std::slice::from_ref(&failed),
        ));
        assert!(selected.contains("failed"));
        assert!(selected.contains("branch exists"));

        // An Available row shows neither the failed tag nor a failure reason.
        let available = projected_session(session, "healthy", "/work/healthy");
        let quiet = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            std::slice::from_ref(&available),
        ));
        assert!(quiet.contains("healthy"));
        assert!(!quiet.contains("failed"));
    }

    #[test]
    fn provider_resume_labels_cover_each_safe_projection_state() {
        let projection = |interrupted, resumable, reason| ProviderResumeProjection {
            interrupted,
            resumable,
            reason,
        };
        assert_eq!(
            resume_label(projection(
                false,
                true,
                ProviderResumeReason::ExplicitResumeAvailable,
            )),
            None
        );
        let unavailable = [
            (
                ProviderResumeReason::ProviderMetadataUnavailable,
                "interrupted · resume unavailable",
            ),
            (
                ProviderResumeReason::AmbiguousProviderMetadata,
                "interrupted · resume metadata ambiguous",
            ),
            (
                ProviderResumeReason::IncompatibleProviderMetadata,
                "interrupted · resume metadata incompatible",
            ),
            (
                ProviderResumeReason::LiveOrOwnershipUnknown,
                "interrupted · resume ownership unknown",
            ),
            (
                ProviderResumeReason::SourceAlreadySuperseded,
                "interrupted · already resumed",
            ),
            (
                ProviderResumeReason::ExplicitResumeAvailable,
                "interrupted · resume unavailable",
            ),
        ];
        for (reason, expected) in unavailable {
            assert_eq!(
                resume_label(projection(true, false, reason)),
                Some(expected)
            );
        }
    }

    #[test]
    fn home_projection_keeps_sessions_and_new_in_identity_order() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let state = AppState::home(workspace, vec![second, first]);
        let snapshot = vec![
            projected_session(first, "same label", "/work/first"),
            projected_session(second, "same label", "/work/second"),
        ];
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &snapshot);

        assert_eq!(
            home.rows(),
            vec![
                Selection::Target(Target::Session(second)),
                Selection::Target(Target::Session(first)),
                Selection::NewSession,
            ]
        );
        let text = joined_home(&home);
        assert!(!text.contains("workspace main"));
        assert!(!text.contains("main  workspace"));
        // Two sidebar rows plus the active-session right-pane header.
        assert_eq!(text.matches("same label").count(), 3);
        assert!(text.contains("+ new session"));
        assert!(!text.contains("+ new session  action"));
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn render_home_draws_the_new_session_row_as_an_inline_name_input() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        // Reach `+ new session` and open its inline create form, then type a name.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        for character in "feature-x".chars() {
            let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
        }
        let text = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[],
        ));
        // The row renders the typed name inline while the form owns input; the
        // static label and the former centered "New session" modal are both gone.
        assert!(text.contains("feature-x"));
        assert!(!text.contains("+ new session"));
        assert!(!text.contains("New session"));
    }

    #[test]
    fn render_home_paints_the_inline_new_affordance_success_with_white_input() {
        // The runtime path that actually draws the inline create form must keep the
        // `+ new:` affordance in the Success (green) role and typed input in the
        // neutral white role, never Accent (cyan).
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        for character in "feature-x".chars() {
            let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
        }
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
        // Render with styles preserved (joined_home strips them).
        let rendered = render_home(30, 100, &home).join("\n");
        assert!(rendered.contains("\u{1b}[1;32m+ new:\u{1b}[0m"));
        assert!(!rendered.contains("\u{1b}[1;36m+ new:"));
        assert!(!rendered.contains("\u{1b}[36m+ new:"));
        assert!(rendered.contains("\u{1b}[1;37mfeature-x"));
        assert!(!rendered.contains("\u{1b}[1;36mfeature-x"));
    }

    #[test]
    fn render_home_draws_a_create_validation_error_inline_on_the_new_session_row() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        // Submitting an empty name keeps the form open and attaches a reducer error,
        // which the inline row surfaces without a modal.
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let text = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[],
        ));
        assert!(text.contains("session name is required"));
        assert!(!text.contains("New session"));
    }

    #[test]
    fn render_home_draws_a_live_invalid_character_error_inline() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        // A disallowed character surfaces the safe rule reminder inline as the user
        // types, without a modal and without discarding the draft name.
        for character in "ok/".chars() {
            let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
        }
        let text = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[],
        ));
        assert!(text.contains("invalid character"));
        assert!(text.contains("ok/"));
    }

    #[test]
    fn new_session_input_lines_draws_only_the_caret_row_without_an_error() {
        let draft = CreateDraft {
            name: "feature-x".into(),
            error: None,
        };
        let lines = new_session_input_lines(30, &draft);
        assert_eq!(lines.len(), 1);
        assert!(strip(&lines[0]).contains("+ new: feature-x"));
        assert_eq!(display_width(&lines[0]), 30);
    }

    #[test]
    fn create_session_projection_renders_the_default_role_picker_line() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::SessionRoleCatalog(SessionRoleCatalog {
                roles: vec![RoleChoice {
                    id: RoleId::new("coder").unwrap(),
                    summary: "Code".to_owned(),
                }],
                default: Some(RoleId::new("coder").unwrap()),
            })),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
        let frame = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[],
        ));
        assert!(frame.contains("role: coder"));
        assert!(frame.contains("↑/↓"));
    }

    #[test]
    fn sidebar_role_badge_uses_safe_id_without_replacing_lifecycle_state() {
        let workspace = WorkspaceId::new();
        let session_id = SessionId::new();
        let mut session = projected_session(session_id, "alpha", "/work/alpha");
        session.role_id = Some("reviewer".to_owned());
        session.lifecycle = SessionLifecycle::Failed;
        session.failure_summary = Some("setup failed".to_owned());
        let state = AppState::home(workspace, vec![session_id]);
        let frame = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[session],
        ));
        assert!(frame.contains("failed"));
        // Failed rows deliberately keep their lifecycle-specific rendering;
        // badges never turn them into an attachable ordinary row.
        assert!(!frame.contains("[reviewer]"));

        let mut available = projected_session(session_id, "alpha", "/work/alpha");
        available.role_id = Some("reviewer".to_owned());
        let mut state = AppState::home(workspace, vec![session_id]);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::SessionRoles(BTreeMap::from([(
                session_id,
                SessionRoleProjection {
                    role_id: Some(RoleId::new("reviewer").unwrap()),
                    role_summary: None,
                },
            )]))),
        );
        let frame = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[available],
        ));
        assert!(frame.contains("[reviewer]"));
    }

    #[test]
    fn new_session_input_lines_paint_the_affordance_success_and_name_white() {
        // The `+ new:` affordance is Success (green) so it stays continuous with the
        // static green `+ new session` label; the typed name is white. No `>`
        // selection chevron is drawn on the input row.
        let draft = CreateDraft {
            name: "feature-x".into(),
            error: None,
        };
        let caret = new_session_input_lines(30, &draft).remove(0);
        // Affordance carries the Success SGR and never the accent one.
        assert!(caret.contains("\u{1b}[1;32m+ new:\u{1b}[0m"));
        assert!(!caret.contains("\u{1b}[1;36m+ new:"));
        // The name span is neutral white, never Accent (cyan).
        assert!(caret.contains("\u{1b}[1;37mfeature-x"));
        assert!(!caret.contains("\u{1b}[1;36mfeature-x"));
        // No selection chevron: the caret text starts at `+ new:` after a blank
        // marker column, never with a `>`.
        let stripped = strip(&caret);
        assert!(stripped.starts_with("  + new: feature-x"));
        assert!(!stripped.trim_start().starts_with('>'));
    }

    #[test]
    fn new_session_input_lines_keep_the_affordance_green_while_an_error_shows_danger() {
        // A live validation error paints Danger below the caret, but the affordance
        // above it stays Success (green) — the two roles coexist.
        let draft = CreateDraft {
            name: "ok".into(),
            error: Some("invalid character".to_string()),
        };
        let lines = new_session_input_lines(24, &draft);
        assert!(lines[0].contains("\u{1b}[1;32m+ new:\u{1b}[0m"));
        assert!(!lines[0].contains("\u{1b}[1;36m+ new:"));
        assert!(lines[1..].iter().any(|line| line.contains("\u{1b}[1;31m")));
    }

    #[test]
    fn new_session_input_lines_wraps_a_long_error_below_the_caret_without_dropping_text() {
        // A safe message wider than the sidebar must wrap across rows rather than
        // being clipped with an ellipsis on the caret line.
        let error = "session name is required and must be unique within the workspace";
        let draft = CreateDraft {
            name: "dup".into(),
            error: Some(error.to_string()),
        };
        let width = 20;
        let lines = new_session_input_lines(width, &draft);
        // Caret row plus one row per wrapped error segment.
        let wrapped = wrap_to_width(error, width);
        assert!(wrapped.len() >= 2, "expected the error to wrap");
        assert_eq!(lines.len(), 1 + wrapped.len());
        // Every row keeps the sidebar column width; nothing overflows.
        assert!(lines.iter().all(|line| display_width(line) == width));
        // The wrap is lossless (no ellipsis / dropped text): the segments rebuild
        // the original message and each rendered error row carries its segment.
        assert_eq!(wrapped.concat(), error);
        assert!(!strip(&lines[0]).contains('…'));
        for (row, segment) in lines[1..].iter().zip(&wrapped) {
            assert!(strip(row).starts_with(segment.as_str()));
        }
    }

    #[test]
    fn new_session_input_lines_wraps_cjk_errors_within_the_display_width() {
        // Full-width glyphs count as two columns; wrapping must not overflow.
        let error = "セッション名が重複しています";
        let draft = CreateDraft {
            name: "重複".into(),
            error: Some(error.to_string()),
        };
        let width = 12;
        let lines = new_session_input_lines(width, &draft);
        let wrapped = wrap_to_width(error, width);
        assert!(wrapped.len() >= 2, "expected the CJK error to wrap");
        assert_eq!(lines.len(), 1 + wrapped.len());
        // Full-width glyphs never push a row past the sidebar width.
        assert!(lines.iter().all(|line| display_width(line) == width));
        // Lossless: the wrapped segments rebuild the original CJK message.
        assert_eq!(wrapped.concat(), error);
    }

    #[test]
    fn new_session_input_lines_keeps_error_rows_styled_without_miscounting_ansi() {
        // The error is wrapped as plain text and painted per row, so each row
        // carries an SGR escape yet still measures at exactly the sidebar width.
        let draft = CreateDraft {
            name: String::new(),
            error: Some("invalid character; use a-z0-9-_".to_string()),
        };
        let width = 16;
        let lines = new_session_input_lines(width, &draft);
        assert!(lines.len() >= 2);
        for line in &lines[1..] {
            assert!(line.contains('\u{1b}'), "error row should carry style");
            assert!(display_width(line) <= width);
        }
    }

    #[test]
    fn new_session_input_lines_survive_a_tiny_width_without_panicking() {
        let draft = CreateDraft {
            name: "name".into(),
            error: Some("too long".to_string()),
        };
        for width in [0usize, 1, 2, 3] {
            let lines = new_session_input_lines(width, &draft);
            assert!(!lines.is_empty());
            assert!(lines.iter().all(|line| display_width(line) <= width));
        }
    }

    #[test]
    fn render_home_keeps_one_footer_and_fits_height_when_a_create_error_wraps() {
        // Reproduce a wrapped inline create error, then confirm the sidebar still
        // renders exactly `height` rows with a single footer — i.e. the row-height
        // accounting matches the lines actually drawn.
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        for character in "bad/name/with/slashes".chars() {
            let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
        }
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
        for height in [6usize, 10, 30] {
            let rows = render_home(height, 20, &home);
            assert_eq!(rows.len(), height);
            let footers = rows
                .iter()
                .filter(|line| strip(line).contains("[switch]"))
                .count();
            assert_eq!(footers, 1, "expected exactly one footer at height {height}");
        }
    }

    fn pr_error() -> SafeError {
        SafeError {
            message: SafeMessage::new("gh unavailable"),
            error_id: "pr".into(),
        }
    }

    #[test]
    fn render_home_replaces_the_frame_with_the_garden_opened_from_overview() {
        let workspace = WorkspaceId::new();
        // controller が集約する phase をすべて踏みつつ、Garden は runtime ごとの
        // phase を失わず投影することを固定する。
        let phases = [
            Some(AgentPhase::Absent),
            Some(AgentPhase::Ready),
            Some(AgentPhase::Running),
            Some(AgentPhase::Waiting),
            Some(AgentPhase::Ended),
        ];
        let ids = phases.iter().map(|_| SessionId::new()).collect::<Vec<_>>();
        let mut state = AppState::home(workspace, ids.clone());
        let mut projected = Vec::new();
        for (index, phase) in phases.iter().enumerate() {
            if let Some(phase) = *phase {
                let _ = update(
                    &mut state,
                    AppEvent::Backend(BackendEvent::RuntimePhase {
                        runtime: runtime_ref(workspace, ids[index]),
                        phase,
                    }),
                );
            }
            projected.push(projected_session(ids[index], &format!("s{index}"), "/work"));
        }
        projected[4].lifecycle = SessionLifecycle::Failed;
        projected[4].failure_summary = Some("worktree missing".to_owned());
        // 同じ session に終了済み runtime があっても、実行中 runtime を Garden から
        // 消さない。sidebar 用の集約は従来どおり Done のままでよい。
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: runtime_ref(workspace, ids[2]),
                phase: AgentPhase::Ended,
            }),
        );
        assert_eq!(state.phase_for(Target::Session(ids[2])), TargetPhase::Done);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into())),
        );
        let home = HomeProjection::from_state(&state, "atlas", Path::new("/work"), &projected);
        let garden = home.garden_sessions.as_ref().expect("garden projection");
        assert!(garden[0].selected);
        assert_eq!(
            garden[4].failure_summary.as_deref(),
            Some("worktree missing")
        );
        assert_eq!(garden[2].agents.len(), 2);
        assert!(
            garden[2]
                .agents
                .iter()
                .any(|agent| agent.phase == AgentPhase::Running)
        );

        let frame = render_home_at(24, 100, &home, now());
        let text = frame
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(frame.len(), 24);
        // Garden が Home を置き換えている（sidebar ではなく庭の footer が出る）。
        assert!(text.contains("Garden · click a usagi to visit · any key to return"));
        assert!(text.contains("running"));
        assert!(text.contains("waiting"));
        assert!(text.contains("1 run · 1 done"));
        assert!(!text.contains("> s0"));
        assert!(text.contains("failed · worktree missing"));
        assert!(text.contains("s0"));

        // 最小サイズに満たない端末では Garden を開かず Home を保つ。操作できる一覧を
        // screen saver で覆わない。
        let small = render_home_at(13, 100, &home, now());
        let small_text = small
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!small_text.contains("any key to return"));
    }

    #[test]
    fn garden_joins_agents_from_inventory_without_overwriting_precise_phases() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let missing_session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        let waiting = runtime_ref(workspace, session);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: waiting.clone(),
                phase: AgentPhase::Waiting,
            }),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into())),
        );

        let live = runtime_ref(workspace, session);
        let missing = runtime_ref(workspace, missing_session);
        let root_terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        let root = AgentRuntimeRef::new(AgentRuntimeId::new(), root_terminal, None)
            .expect("a root runtime owns a root terminal");
        let item = |runtime, state| AgentRuntimeInventoryItem {
            runtime,
            continuation: AgentContinuationRef::new(),
            state,
            resumed_from: None,
        };
        let inventory = AgentInventory {
            workspace_id: workspace,
            runtimes: vec![
                // The duplicate inventory row must not flatten Waiting to Live.
                item(waiting.clone(), AgentRuntimeInventoryState::Live),
                item(live.clone(), AgentRuntimeInventoryState::Live),
                // Inventory cannot create a plot for a session absent from Home.
                item(missing, AgentRuntimeInventoryState::Live),
                // Workspace-root Agents have no session plot.
                item(root, AgentRuntimeInventoryState::Live),
            ],
            resumable: Vec::new(),
        };
        let home = HomeProjection::from_state(
            &state,
            "atlas",
            Path::new("/work"),
            &[projected_session(session, "garden", "/work/garden")],
        )
        .with_agent_inventory(Some(&inventory));

        let garden = home.garden_sessions.as_ref().expect("garden projection");
        assert_eq!(garden.len(), 1);
        assert_eq!(garden[0].agents.len(), 2);
        assert!(garden[0].agents.contains(&widgets::garden::GardenAgent {
            runtime_id: waiting.agent_runtime_id,
            phase: AgentPhase::Waiting,
        }));
        assert!(garden[0].agents.contains(&widgets::garden::GardenAgent {
            runtime_id: live.agent_runtime_id,
            phase: AgentPhase::Running,
        }));
        let text = strip(&render_home_at(24, 100, &home, now()).join("\n"));
        assert!(text.contains("1 run · 1 wait"));
        assert!(!text.contains("no agents"));
    }

    #[test]
    fn garden_maps_every_inventory_state_to_a_visible_phase() {
        let cases = [
            (AgentRuntimeInventoryState::Reserved, AgentPhase::Ready),
            (AgentRuntimeInventoryState::Live, AgentPhase::Running),
            (
                AgentRuntimeInventoryState::Interrupted,
                AgentPhase::Interrupted,
            ),
            (
                AgentRuntimeInventoryState::Unavailable,
                AgentPhase::Interrupted,
            ),
            (AgentRuntimeInventoryState::Exited, AgentPhase::Exited),
            (AgentRuntimeInventoryState::Reclaimed, AgentPhase::Exited),
        ];
        for (state, expected) in cases {
            assert_eq!(super::garden_inventory_phase(state), expected);
        }
    }

    /// 自動表示は renderer が庭を描ける端末サイズでだけ許す。判定は
    /// [`render_home_at`] の縮退（0 は fallback サイズ）と同じ正規化で行う。
    #[test]
    fn garden_fits_matches_the_size_the_renderer_accepts() {
        assert!(garden_fits(24, 100));
        assert!(garden_fits(
            widgets::garden::MIN_HEIGHT,
            widgets::garden::MIN_WIDTH
        ));
        assert!(!garden_fits(widgets::garden::MIN_HEIGHT - 1, 100));
        assert!(!garden_fits(24, widgets::garden::MIN_WIDTH - 1));
        // 0 は「未知」であって「狭い」ではない。frame と同じ fallback (24x80) で判定する。
        assert!(garden_fits(0, 0));
    }

    /// click 解決は frame と同じ layout 呼び出しの hitbox に当てる。うさぎに当たれば
    /// その plot に束縛された stable `SessionId`、外れれば wake-up。
    #[test]
    fn a_garden_click_resolves_against_the_drawn_plots() {
        let workspace = WorkspaceId::new();
        let ids = (0..3).map(|_| SessionId::new()).collect::<Vec<_>>();
        let mut state = AppState::home(workspace, ids.clone());
        let projected = ids
            .iter()
            .enumerate()
            .map(|(index, id)| projected_session(*id, &format!("s{index}"), "/work"))
            .collect::<Vec<_>>();
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into())),
        );
        let home = HomeProjection::from_state(&state, "atlas", Path::new("/work"), &projected);

        let frame = garden_frame(24, 100, &home, now()).expect("the garden owns this frame");
        assert_eq!(frame.hitboxes.len(), 3);
        for hitbox in &frame.hitboxes {
            let column = u16::try_from(hitbox.column + hitbox.width / 2).expect("fits a u16");
            let row = u16::try_from(hitbox.row + hitbox.height / 2).expect("fits a u16");
            assert_eq!(
                garden_click_at(24, 100, &home, now(), column, row),
                Some(GardenClick::Visit(hitbox.session_id)),
                "the centre of a plot is its own usagi"
            );
        }

        // 庭の余白（footer 行）はうさぎではないので wake-up になる。
        assert_eq!(
            garden_click_at(24, 100, &home, now(), 0, 23),
            Some(GardenClick::Dismiss)
        );

        // Garden が frame でない場合は `None` を返し、呼び出し側は通常の Home の
        // hit test を続ける（overlay が閉じている場合と、庭が収まらない端末）。
        let plain = HomeProjection::from_state(
            &AppState::home(workspace, ids.clone()),
            "atlas",
            Path::new("/work"),
            &projected,
        );
        assert_eq!(garden_click_at(24, 100, &plain, now(), 10, 10), None);
        assert_eq!(garden_click_at(13, 100, &home, now(), 10, 10), None);
    }

    #[test]
    fn the_garden_tick_advances_with_the_frame_clock() {
        let base = now();
        assert_eq!(garden_tick(base), garden_tick(base));
        // 1 秒で pose が 1 つ進み、Garden animation cycle で一周する。frame material の壁時計は秒へ
        // 切り捨てられるので、これが観測できる最小の刻みである。
        assert_ne!(
            garden_tick(base),
            garden_tick(base + chrono::Duration::seconds(1))
        );
        assert_eq!(
            garden_tick(base),
            garden_tick(
                base + chrono::Duration::seconds(
                    i64::try_from(widgets::garden::ANIMATION_CYCLE_TICKS)
                        .expect("Garden cycle fits i64"),
                )
            )
        );
        // 秒未満は同じ frame（間引きと同じ解像度）。
        assert_eq!(
            garden_tick(base),
            garden_tick(base + chrono::Duration::milliseconds(400))
        );
    }

    #[test]
    fn garden_material_clock_collapses_held_poses_and_all_reduced_motion_ticks() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: runtime_ref(workspace, session),
                phase: AgentPhase::Running,
            }),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("garden".into())),
        );
        let home = HomeProjection::from_state(
            &state,
            "atlas",
            Path::new("/work"),
            &[projected_session(session, "running", "/work")],
        );
        let epoch = DateTime::from_timestamp(0, 0).expect("Unix epoch");
        let canonical = (0..widgets::garden::ANIMATION_CYCLE_TICKS)
            .map(|tick| {
                home.canonical_garden_now(
                    24,
                    100,
                    epoch + chrono::Duration::seconds(i64::try_from(tick).expect("tick fits i64")),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            canonical.windows(2).any(|ticks| ticks[0] != ticks[1]),
            "visible motion must advance the material clock"
        );
        assert!(
            canonical.windows(2).any(|ticks| ticks[0] == ticks[1]),
            "held poses should collapse to the same material clock"
        );

        let reduced = home.with_garden_reduced_motion(true);
        assert_eq!(
            reduced.canonical_garden_now(24, 100, epoch),
            reduced.canonical_garden_now(24, 100, epoch + chrono::Duration::seconds(5))
        );

        let ordinary_home_now = epoch + chrono::Duration::days(20_000);
        assert_eq!(
            reduced.canonical_garden_now(13, 63, ordinary_home_now),
            ordinary_home_now,
            "a Garden that does not fit must preserve the ordinary Home clock"
        );
    }

    #[test]
    fn render_home_composes_the_daemon_status_opened_from_overview() {
        let workspace = WorkspaceId::new();
        let known_session = SessionId::new();
        let missing_session = SessionId::new();
        let mut state = AppState::home(workspace, vec![known_session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("daemon".into())),
        );
        let metrics = DaemonMetrics {
            schema_version: 3,
            sampled_at_ms: u64::try_from(now().timestamp_millis()).unwrap(),
            cpu_percent_hundredths: 100,
            resident_memory_bytes: 32 * MEBIBYTE,
            active_subscribers: 1,
            dropped_updates: 0,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: Some(AgentConcurrency {
                in_use: 16,
                limit: 16,
            }),
            failed_background_workers: 0,
        };
        let runtime_item = |session_id| {
            let runtime_id = AgentRuntimeId::new();
            let terminal = TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id,
                worktree_id: WorktreeId::new(),
            };
            (
                runtime_id,
                AgentRuntimeInventoryItem {
                    runtime: AgentRuntimeRef::new(runtime_id, terminal, session_id).unwrap(),
                    continuation: AgentContinuationRef::new(),
                    state: AgentRuntimeInventoryState::Live,
                    resumed_from: None,
                },
            )
        };
        let (runtime_id, root_runtime) = runtime_item(None);
        let (_, known_runtime) = runtime_item(Some(known_session));
        let (_, missing_runtime) = runtime_item(Some(missing_session));
        let inventory = AgentInventory {
            workspace_id: workspace,
            runtimes: vec![root_runtime, known_runtime, missing_runtime],
            resumable: Vec::new(),
        };
        let sessions = [projected_session(
            known_session,
            "known-session",
            "/work/known-session",
        )];
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions)
            .with_metrics(Some(metrics))
            .with_agent_inventory(Some(&inventory));
        let frame = strip(&render_home_at(24, 100, &home, now()).join("\n"));
        assert!(frame.contains("Daemon"));
        assert!(frame.contains("16/16  saturated"));
        assert!(frame.contains(&format!(
            "root  live  #{}",
            short_id(&runtime_id.to_string())
        )));
        assert!(frame.contains("known-session  live"));
        assert!(frame.contains(&format!(
            "session #{}  live",
            short_id(&missing_session.to_string())
        )));
        assert!(frame.contains("Ctrl-D"));
    }

    #[test]
    fn render_home_draws_the_pr_overlay_at_its_selection() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('p')));
        let mut first = PrLink::new(7, "https://github.com/o/r/pull/7");
        first.title = Some("add feature".into());
        let mut second = PrLink::new(8, "https://github.com/o/r/pull/8");
        second.title = Some("fix bug".into());
        second.state = PrState::Merged;
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target,
                revision: 1,
                prs: vec![first, second],
            }),
        );
        // Move the cursor to the second PR; selection stays in the list instead
        // of creating a duplicate detail block below it.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        );
        let text = joined_home(&home);
        assert!(text.contains("Pull Request"));
        assert!(text.contains("#7"));
        assert!(text.contains("add feature"));
        assert!(text.contains("merged"));
        assert_eq!(text.matches("#8").count(), 1);
        assert!(!text.contains("github.com/o/r/pull/8"));

        // Closing the modal leaves the same daemon projection visible as the
        // sidebar badge; no legacy SessionRecord PR data is required.
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        );
        assert!(strip(&render_home(30, 100, &home).join("\n")).contains(&format!("{PR_ICON} 2")));
    }

    #[test]
    fn render_home_draws_a_pr_fetch_error_as_a_safe_notice() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsError {
                target,
                error: pr_error(),
            }),
        );
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        );
        let text = joined_home(&home);
        assert!(text.contains("Pull Request"));
        assert!(text.contains("gh unavailable"));
    }

    #[test]
    fn render_home_draws_the_preview_overlay_and_its_error() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('v')));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target,
                lines: vec!["# Heading".into(), "content line".into()],
            }),
        );
        let ready = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        ));
        assert!(ready.contains("Preview"));
        assert!(ready.contains("Heading"));
        assert!(ready.contains("content line"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewError {
                target,
                error: SafeError {
                    message: SafeMessage::new("no preview available"),
                    error_id: "preview".into(),
                },
            }),
        );
        let errored = joined_home(&HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        ));
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
        // the reservation the hit-test assumes stays constant — including the
        // second row the Agent concurrency projection occupies.
        let metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 3,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: Some(AgentConcurrency {
                in_use: 1,
                limit: 16,
            }),
            failed_background_workers: 0,
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
    fn switch_draws_only_the_selected_cursor_and_not_the_previous_target_rail() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let mut state = AppState::home(workspace, vec![first, second]);
        // The first session is active; move only the Switch cursor to second.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[
                projected_session(first, "first", "/work/first"),
                projected_session(second, "second", "/work/second"),
            ],
        );

        let lines = render_home(30, 100, &home)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>();
        assert!(lines.iter().all(|line| !line.contains("| first")));
        assert!(lines.iter().any(|line| line.contains("\u{f0907} second")));
        let text = joined_home(&home);
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn home_projection_never_marks_new_as_active_and_refresh_falls_back_to_root_cwd() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let mut projected = projected_session(session, "session", "/work/session");
        projected.has_notes = true;
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[projected]);
        let text = joined_home(&home);
        assert!(!text.contains("> + new session"));
        assert!(!text.contains("| + new session"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(Vec::new())),
        );
        let refreshed = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
        // `+ new` は常設 action row のため refresh で消えない。一方、消えた active
        // session は typed identity で検出され active なしへ縮退する。
        assert_eq!(state.selected(), Selection::NewSession);
        assert_eq!(state.active(), None);
        assert!(joined_home(&refreshed).contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
    fn home_projection_uses_v1_marker_precedence_and_hides_cursor_in_closeup() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let mut state = AppState::home(workspace, vec![first, second]);
        // Activate first, then move the cursor to second without changing the current target.
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let snapshot = [
            projected_session(first, "同じ名前", "/work/first"),
            projected_session(second, "同じ名前", "/work/second"),
        ];

        let closeup = HomeProjection::from_state(&state, "work", Path::new("/work"), &snapshot);
        let closeup_text = joined_home(&closeup);
        assert!(closeup_text.contains("| 同じ名前"));
        assert!(!closeup_text.contains("\u{f0907} 同じ名前"));
        assert!(closeup_text.contains("[closeup] Ctrl-O:"));
        assert!(closeup_text.contains("x/Ctrl-X close"));
        let closeup_rendered = render_home(30, 100, &closeup).join("\n");
        assert!(closeup_rendered.contains("\u{1b}[1;36m同じ名前\u{1b}[0m"));
        assert!(closeup_rendered.contains("\u{1b}[36m同じ名前\u{1b}[0m"));

        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlO));
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        let switch = HomeProjection::from_state(&state, "work", Path::new("/work"), &snapshot);
        let switch_text = joined_home(&switch);
        assert!(!switch_text.contains("| 同じ名前"));
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
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlO));
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
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
    fn switch_dims_nonselected_session_metadata_after_ansi_role_resets() {
        let workspace = WorkspaceId::new();
        let active = SessionId::new();
        let selected = SessionId::new();
        let mut state = AppState::home(workspace, vec![active, selected]);
        // Make `active` the current target, move the cursor to `selected`, then
        // return to Switch. The previous target no longer keeps a green rail,
        // and its whole metadata row remains inactive.
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlO));
        let mut active_session = projected_session(active, "active", "/work/active");
        active_session.last_modified = Utc::now();
        active_session.pr_summary = Some(format!("{PR_ICON} 2"));
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[
                active_session,
                projected_session(selected, "selected", "/work/selected"),
            ],
        )
        .with_git_diffs(&BTreeMap::from([(
            active,
            GitDiff {
                base: "origin/main".to_owned(),
                ahead: 1,
                behind: 2,
                added: 3,
                removed: 4,
            },
        )]));

        let metadata = render_home(30, 100, &home)
            .into_iter()
            .find(|line| line.contains("now"))
            .expect("active session metadata row");

        assert!(metadata.contains("\u{1b}[2m  now"));
        assert!(!metadata.contains("\u{1b}[1;32m|"));
        assert!(metadata.contains("\u{1b}[2;36m↑1"));
        assert!(metadata.contains("\u{1b}[2;35m↓2"));
        assert!(metadata.contains(&format!("{PR_ICON} 2")));
        assert!(metadata.contains("\u{1b}[2;32m+ 3"));
        assert!(metadata.contains("\u{1b}[2;31m- 4"));
        assert!(!metadata.contains("\u{1b}[0m now"));
    }

    #[test]
    fn switch_keeps_selected_session_git_status_bright() {
        let workspace = WorkspaceId::new();
        let selected = SessionId::new();
        let state = AppState::home(workspace, vec![selected]);
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(selected, "selected", "/work/selected")],
        )
        .with_git_diffs(&BTreeMap::from([(
            selected,
            GitDiff {
                base: "origin/main".to_owned(),
                ahead: 1,
                behind: 2,
                added: 3,
                removed: 4,
            },
        )]));

        let metadata = render_home(30, 100, &home)
            .into_iter()
            .find(|line| line.contains("↑1"))
            .expect("selected session metadata row");

        assert!(metadata.contains("\u{1b}[36m↑1"));
        assert!(metadata.contains("\u{1b}[35m↓2"));
        assert!(metadata.contains("\u{1b}[32m+ 3"));
        assert!(metadata.contains("\u{1b}[31m- 4"));
        assert!(!metadata.contains("\u{1b}[2;36m↑1"));
        assert!(!metadata.contains("\u{1b}[2;35m↓2"));
        assert!(!metadata.contains("\u{1b}[2;32m+ 3"));
        assert!(!metadata.contains("\u{1b}[2;31m- 4"));
    }

    #[test]
    fn closeup_keeps_elapsed_time_at_normal_intensity() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        )
        .with_git_diffs(&BTreeMap::from([(
            session,
            GitDiff {
                base: "origin/main".to_owned(),
                ahead: 1,
                behind: 0,
                added: 2,
                removed: 0,
            },
        )]));

        let metadata = render_home_at(30, 100, &home, now())
            .into_iter()
            .find(|line| line.contains("now"))
            .expect("closeup session metadata row");

        assert!(metadata.contains("\u{1b}[0m now"));
        assert!(!metadata.contains("\u{1b}[2m now"));
        assert!(!metadata.contains("\u{1b}[2;37mnow"));
    }

    #[test]
    fn delete_failure_row_is_compact_and_does_not_expose_the_backend_summary() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let mut failed = projected_session(session, "feature", "/work/feature");
        failed.lifecycle = SessionLifecycle::Failed;
        failed.failure_stage = Some(FailureStage::Delete);
        failed.failure_summary = Some("git worktree remove failed: private detail".to_owned());
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[failed]);

        let frame = render_home_at(30, 100, &home, now()).join("\n");

        assert!(frame.contains("\u{1b}[1;31mfeature\u{1b}[0m"));
        assert!(strip(&frame).contains("remove failed"));
        assert!(!frame.contains("private detail"));
        assert!(!strip(&frame).contains("feature  failed"));
    }

    #[test]
    fn switch_paints_the_selected_new_session_row_success_not_accent() {
        let workspace = WorkspaceId::new();
        let state = AppState::home(workspace, Vec::new());
        // An empty Home rests the Switch cursor on `+ new session`.
        assert_eq!(state.selected(), Selection::NewSession);
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);

        // Regression (#376): the Switch cursor on `+ new session` must keep the
        // Success (green) role and only add bold — never fall through to the
        // accent (cyan) `selected` branch used by real targets.
        let rendered = render_home(30, 100, &home).join("\n");
        assert!(rendered.contains("\u{1b}[1;32m+ new session\u{1b}[0m"));
        assert!(!rendered.contains("\u{1b}[1;36m+ new session\u{1b}[0m"));
        assert!(!rendered.contains("\u{1b}[36m+ new session\u{1b}[0m"));
        // Unlike other Switch rows, the new-session action stays chevron-free
        // while it is focused.
        assert!(
            render_home(30, 100, &home)
                .iter()
                .map(|line| strip(line))
                .any(|line| line.contains("  + new session"))
        );
        assert!(
            render_home(30, 100, &home)
                .iter()
                .map(|line| strip(line))
                .all(|line| !line.contains("> + new session"))
        );
    }

    #[test]
    fn closeup_paints_the_new_session_row_success_without_bold() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        // Activate the session to enter Closeup; the persistent `+ new session`
        // row still renders and, outside Switch, carries no cursor.
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        );

        // Regression (#376): bold is reserved for the Switch cursor, so Closeup
        // keeps `+ new session` Success (green) but unbolded, and never accent.
        let rendered = render_home(30, 100, &home).join("\n");
        assert!(rendered.contains("\u{1b}[32m+ new session\u{1b}[0m"));
        assert!(!rendered.contains("\u{1b}[1;32m+ new session\u{1b}[0m"));
        assert!(!rendered.contains("\u{1b}[1;36m+ new session\u{1b}[0m"));
        assert!(!rendered.contains("\u{1b}[36m+ new session\u{1b}[0m"));
    }

    #[test]
    fn home_projection_handles_tiny_geometry_without_an_active_session() {
        let workspace = WorkspaceId::new();
        let state = AppState::home(workspace, Vec::new());
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);

        let zero_body = render_home(2, 20, &home);
        let one_row_body = render_home(3, 20, &home);
        assert_eq!(zero_body.len(), 2);
        assert_eq!(one_row_body.len(), 3);
        assert!(joined_home(&home).contains("No tabs stirring yet. Enter starts one."));

        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let home = HomeProjection::from_state(
            &state,
            "work",
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        );
        assert_eq!(render_home(6, 20, &home).len(), 6);
    }

    #[test]
    fn home_sidebar_mascot_animates_only_on_tick_and_stays_in_the_background() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let initial = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
        let first = render_home(20, 80, &initial).join("\n");
        assert!(strip(&first).contains("(o.o)?"));

        for _ in 0..4 {
            let _ = update(&mut state, AppEvent::Tick);
        }
        let blink = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
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
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[])
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
            schema_version: 3,
            sampled_at_ms: 1,
            cpu_percent_hundredths: 0,
            resident_memory_bytes: 0,
            active_subscribers: 1,
            dropped_updates: 0,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: Some(AgentConcurrency {
                in_use: 0,
                limit: 16,
            }),
            failed_background_workers: 0,
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
            schema_version: 3,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: Some(AgentConcurrency {
                in_use: 3,
                limit: 16,
            }),
            failed_background_workers: 0,
        };

        // The daemon observation flows through `with_metrics` into the sidecar row
        // beside usagi.
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let home = HomeProjection::from_state(&state, "actual", Path::new("/tmp/actual"), &[])
            .with_metrics(Some(metrics));
        let controller = render_home(30, 100, &home);

        let controller_row = controller
            .iter()
            .find(|line| line.contains('\u{f2db}'))
            .expect("daemon metric row beside usagi");

        // The row carries both glyphs and the v1 CPU/memory summary text.
        assert!(strip(controller_row).contains("\u{f2db} 1%    \u{f233} 45MB"));

        // The Agent concurrency the daemon admits from sits on its own row below,
        // as `in use / limit`.
        let concurrency_row = controller
            .iter()
            .find(|line| line.contains(AGENT_ICON))
            .expect("agent concurrency row beside usagi");
        assert!(strip(concurrency_row).contains("\u{f085} 3/16"));
    }

    /// The concurrency row reports the daemon's own admission level, so the three
    /// things a viewer must be able to tell apart — idle, saturated, and "the
    /// daemon did not say" — are visibly different.
    #[test]
    fn agent_concurrency_row_separates_idle_saturated_and_unreported() {
        let row = |concurrency| strip(&super::agent_concurrency_row(concurrency));

        // Zero is a reported level, not an absence.
        assert_eq!(
            row(Some(AgentConcurrency {
                in_use: 0,
                limit: 16
            })),
            "\u{f085} 0/16"
        );
        // A daemon that reports nothing is a dash, which cannot be read as zero.
        assert_eq!(row(None), "\u{f085} —");

        // Colour escalates with the level and is strongest once the next launch
        // would be refused.
        let calm = super::agent_concurrency_row(Some(AgentConcurrency {
            in_use: 1,
            limit: 16,
        }));
        let busy = super::agent_concurrency_row(Some(AgentConcurrency {
            in_use: 12,
            limit: 16,
        }));
        let full = super::agent_concurrency_row(Some(AgentConcurrency {
            in_use: 16,
            limit: 16,
        }));
        assert_eq!(strip(&full), "\u{f085} 16/16");
        assert_ne!(calm, busy);
        assert_ne!(busy, full);
        assert!(full.contains(&Role::Danger.style().paint("\u{f085} 16/16")));
        assert!(busy.contains(&Role::Warning.style().paint("\u{f085} 12/16")));
        // The unreported dash stays as quiet as a calm level.
        assert_eq!(
            super::agent_concurrency_row(None).contains("\u{1b}[2m"),
            calm.contains("\u{1b}[2m")
        );
    }

    /// A daemon older than the projection keeps the frame it drew before: the
    /// concurrency row degrades to a dash instead of blanking the sidecar or
    /// inventing a count.
    #[test]
    fn home_sidecar_degrades_when_the_daemon_omits_agent_concurrency() {
        let mut metrics = usagi_core::usecase::client::DaemonMetrics {
            schema_version: 3,
            sampled_at_ms: 42,
            cpu_percent_hundredths: 123,
            resident_memory_bytes: 45 * 1_048_576,
            active_subscribers: 3,
            dropped_updates: 5,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: None,
            failed_background_workers: 0,
        };
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let render = |metrics: &usagi_core::usecase::client::DaemonMetrics| {
            render_home(
                30,
                100,
                &HomeProjection::from_state(&state, "actual", Path::new("/tmp/actual"), &[])
                    .with_metrics(Some(metrics.clone())),
            )
        };
        let unreported = render(&metrics);
        let row = unreported
            .iter()
            .find(|line| line.contains(AGENT_ICON))
            .expect("agent concurrency row is still drawn");
        assert!(strip(row).contains("\u{f085} —"));
        // The CPU/memory row is unaffected by the missing projection.
        assert!(
            unreported
                .iter()
                .any(|line| strip(line).contains("\u{f2db} 1%    \u{f233} 45MB"))
        );

        // Reporting a level changes only that row's content, not the frame's shape.
        metrics.agent_concurrency = Some(AgentConcurrency {
            in_use: 2,
            limit: 16,
        });
        let reported = render(&metrics);
        assert_eq!(reported.len(), unreported.len());
        assert!(
            reported
                .iter()
                .any(|line| strip(line).contains("\u{f085} 2/16"))
        );
    }

    /// A sidebar too narrow for the numbers clips them with the rest of the row
    /// rather than overflowing into the right pane.
    #[test]
    fn narrow_sidebar_clips_the_agent_concurrency_row() {
        use crate::presentation::widgets::mascot::sidebar_block_with_sidecar;
        use crate::usecase::application::controller::{
            SIDEBAR_MASCOT_MIN_LEFT, SIDEBAR_MASCOT_ROWS,
        };
        let sidecar = vec![
            super::agent_concurrency_row(Some(AgentConcurrency {
                in_use: 16,
                limit: 16,
            })),
            "\u{f2db} 1%    \u{f233} 45MB".to_owned(),
        ];
        let block = sidebar_block_with_sidecar(SIDEBAR_MASCOT_MIN_LEFT, 0, None, &sidecar)
            .expect("the rabbit fits its minimum width");
        for row in block.rows() {
            assert_eq!(
                crate::presentation::widgets::display_width(&strip(row)),
                SIDEBAR_MASCOT_MIN_LEFT,
                "every mascot row is clipped to the sidebar width"
            );
        }
        // At the minimum width the numbers do not fit beside the rabbit at all,
        // so the row is clipped rather than wrapped into an extra line.
        assert_eq!(block.reserved_rows(), SIDEBAR_MASCOT_ROWS);
        assert!(!strip(&block.rows().join("\n")).contains("16/16"));
    }

    #[test]
    fn home_without_metrics_keeps_the_pre_metrics_frame() {
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
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

    // ── daemon health indicator ─────────────────────────────────────────────
    //
    // 判定そのものは `usagi_core::usecase::daemon_health` の単体テストが固定する。
    // ここで固定するのは「正常時は何も足さない」「異常時は短い安全な理由を出す」
    // 「狭幅で溢れない」という描画側の契約である。

    fn epoch_ms(clock: DateTime<Utc>) -> u64 {
        u64::try_from(clock.timestamp_millis()).expect("test clock is after the epoch")
    }

    fn health_metrics(sampled_at_ms: u64) -> usagi_core::usecase::client::DaemonMetrics {
        usagi_core::usecase::client::DaemonMetrics {
            schema_version: 3,
            sampled_at_ms,
            cpu_percent_hundredths: 120,
            resident_memory_bytes: 45 * MEBIBYTE,
            active_subscribers: 1,
            dropped_updates: 0,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: Some(AgentConcurrency {
                in_use: 2,
                limit: 16,
            }),
            failed_background_workers: 0,
        }
    }

    fn observed_at(sampled_at_ms: u64) -> DaemonHealthTracker {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&health_metrics(sampled_at_ms));
        tracker
    }

    fn health_home(clock: DateTime<Utc>) -> HomeProjection {
        let state = AppState::home(WorkspaceId::new(), Vec::new());
        HomeProjection::from_state(&state, "work", Path::new("/work"), &[])
            .with_metrics(Some(health_metrics(epoch_ms(clock))))
    }

    #[test]
    fn a_healthy_daemon_leaves_the_home_frame_untouched() {
        let clock = now();
        let home = health_home(clock);
        let baseline = render_home_at(30, 100, &home, clock);

        // 観測はあるが劣化していない = indicator を出さない。
        let healthy = home.clone().with_health(observed_at(epoch_ms(clock)));
        assert_eq!(render_home_at(30, 100, &healthy, clock), baseline);
        // 一度も観測していない既定値も同じ（daemon 不在の workspace は正常）。
        assert_eq!(
            render_home_at(
                30,
                100,
                &home.with_health(DaemonHealthTracker::default()),
                clock
            ),
            baseline
        );
        assert!(
            !baseline.iter().any(|line| line.contains(HEALTH_GLYPH)),
            "healthy home shows no health indicator"
        );
    }

    #[test]
    fn a_stalled_lane_warns_and_a_silent_daemon_is_danger() {
        let clock = now();
        let home = health_home(clock);
        let baseline = render_home_at(30, 100, &home, clock);

        let stalled = render_home_at(
            30,
            100,
            &home
                .clone()
                .with_health(observed_at(epoch_ms(clock) - 10_000)),
            clock,
        );
        let warned = stalled
            .iter()
            .find(|line| line.contains(HEALTH_GLYPH))
            .expect("stalled metrics draw the indicator");
        assert!(strip(warned).contains("metrics 停滞"));
        assert!(warned.contains(&Role::Warning.style().bold().paint("⚠ metrics 停滞")));

        // 30s 以上無音なら danger。理由も style も入れ替わる。
        let silent = render_home_at(
            30,
            100,
            &home.with_health(observed_at(epoch_ms(clock) - 40_000)),
            clock,
        );
        let danger = silent
            .iter()
            .find(|line| line.contains(HEALTH_GLYPH))
            .expect("an unresponsive daemon draws the indicator");
        assert!(strip(danger).contains("daemon 無応答"));
        assert!(danger.contains(&Role::Danger.style().bold().paint("⚠ daemon 無応答")));

        // sidecar は mascot の予約行を増やさないため、書き換わる行は 1 行だけである
        // （session 行・viewport・footer は動かない）。
        let rewritten = baseline
            .iter()
            .zip(&stalled)
            .filter(|(quiet, warned)| quiet != warned)
            .count();
        assert_eq!(
            rewritten, 1,
            "the indicator rewrote more than the sidecar row"
        );
        assert_eq!(baseline.len(), stalled.len());
    }

    #[test]
    fn the_indicator_degrades_on_a_narrow_sidebar() {
        let health = DaemonHealth::Warning(HealthReason::MetricsStalled);
        let full = health_badge(health, LEFT_WIDTH).expect("the label fits the sidebar");
        assert!(strip(&full).contains("metrics 停滞"));

        // 文言が入らない幅では記号だけに縮退する。
        let narrow = health_badge(health, SIDECAR_GUTTER + 3).expect("the glyph still fits");
        assert_eq!(strip(&narrow), HEALTH_GLYPH.to_string());
        // 記号すら置けない幅では行を出さない（溢れさせない）。
        assert_eq!(health_badge(health, SIDECAR_GUTTER), None);
        assert_eq!(health_badge(health, 0), None);
        // 正常時は行そのものが無い。
        assert_eq!(health_badge(DaemonHealth::Ok, LEFT_WIDTH), None);
    }

    #[test]
    fn the_indicator_appears_without_a_metrics_observation() {
        let unresponsive = DaemonHealth::Danger(HealthReason::DaemonUnresponsive);
        // metrics unavailable でも indicator だけは出す。
        let alone = sidecar_labels(
            LEFT_WIDTH,
            None,
            unresponsive,
            SessionStateCounts::default(),
        );
        assert_eq!(alone.len(), 1);
        assert!(strip(&alone[0]).contains("daemon 無応答"));

        // 観測があれば badge の下に Agent concurrency 行と CPU / メモリ行が続く。
        let metrics = health_metrics(1);
        let both = sidecar_labels(
            LEFT_WIDTH,
            Some(&metrics),
            DaemonHealth::Warning(HealthReason::PrScanIncomplete),
            SessionStateCounts::default(),
        );
        assert_eq!(both.len(), 3);
        assert!(strip(&both[0]).contains("PR 検出の欠落"));
        assert!(strip(&both[1]).contains("2/16"));
        assert!(strip(&both[2]).contains("45MB"));

        // 供給元が 4 つとも語るときは Agent concurrency 行が譲り、sidecar の上限
        // （うさぎの 3 行）に収まる。順序は health → session 件数 → metrics であり、
        // 異常時の frame は indicator 導入時と同じ 3 行のままになる。
        let full = sidecar_labels(
            LEFT_WIDTH,
            Some(&metrics),
            DaemonHealth::Warning(HealthReason::TerminalBackpressure),
            SessionStateCounts {
                running: 2,
                waiting: 1,
                failed: 0,
            },
        );
        assert_eq!(full.len(), 3);
        assert!(strip(&full[0]).contains("端末出力の滞留"));
        assert!(strip(&full[1]).contains("run 2"));
        assert!(strip(&full[2]).contains("45MB"));
        // 落ちるのは concurrency 行だけで、widget の `take(3)` に最下段を捨てさせない。
        assert!(full.iter().all(|row| !strip(row).contains(AGENT_ICON)));

        // health が静かなら 4 つ目の枠が空くので concurrency 行は残る。
        let calm = sidecar_labels(
            LEFT_WIDTH,
            Some(&metrics),
            DaemonHealth::Ok,
            SessionStateCounts {
                running: 2,
                waiting: 1,
                failed: 0,
            },
        );
        assert_eq!(calm.len(), 3);
        assert!(strip(&calm[0]).contains("run 2"));
        assert!(strip(&calm[1]).contains("2/16"));
        assert!(strip(&calm[2]).contains("45MB"));

        // 正常時の sidecar は health 導入前とバイト単位で同じ 1 行である。
        assert_eq!(
            sidecar_labels(
                LEFT_WIDTH,
                Some(&metrics),
                DaemonHealth::Ok,
                SessionStateCounts::default()
            ),
            super::mascot_metrics(Some(&metrics), 0)
        );
        assert!(
            sidecar_labels(
                LEFT_WIDTH,
                None,
                DaemonHealth::Ok,
                SessionStateCounts::default()
            )
            .is_empty()
        );
    }

    #[test]
    fn every_health_reason_has_a_short_label_without_raw_detail() {
        for reason in [
            HealthReason::DaemonUnresponsive,
            HealthReason::MetricsStalled,
            HealthReason::TerminalOutputDropped,
            HealthReason::TerminalBackpressure,
            HealthReason::PrScanIncomplete,
            HealthReason::MetricsUpdatesDropped,
            HealthReason::BackgroundWorkerStopped,
        ] {
            let label = health_reason_label(reason);
            let badge = health_badge(DaemonHealth::Warning(reason), LEFT_WIDTH)
                .expect("every reason fits the sidebar at its intended width");
            assert!(strip(&badge).contains(label));
            // 既定幅では縮退しない（= 予算に収まる）。
            assert!(
                display_width(&format!("{HEALTH_GLYPH} {label}")) <= LEFT_WIDTH - SIDECAR_GUTTER,
                "{label} does not fit the sidecar budget"
            );
            // path・改行・ANSI・生の出力を含まない短い語である。
            assert!(!label.contains('/'));
            assert!(!label.contains('\u{1b}'));
            assert!(!label.contains('\n'));
        }
    }

    #[test]
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
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
        let text = joined_home(&home);
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
        assert!(text.contains("feedback: operation error: Session creation failed (err-safe-7)"));
        assert!(!text.contains("daemon internal detail: token=secret"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::Disconnected)),
        );
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
        let text = joined_home(&home);
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
        assert!(text.contains("feedback: disconnected; reconnect to continue"));
    }

    #[test]
    fn abnormal_daemon_speech_projects_only_non_healthy_states() {
        // MascotSpeech は内容を公開しないため、bubble を描画して stripped text を検査する。
        let bubble = |feedback: &Feedback| {
            let speech = abnormal_daemon_speech(Some(feedback)).expect("abnormal state speaks");
            strip(
                &crate::presentation::widgets::mascot::sidebar_block_with_sidecar(
                    40,
                    0,
                    Some(&speech),
                    &[],
                )
                .expect("mascot fits")
                .rows()
                .join("\n"),
            )
        };

        // 正常系（無 feedback・進行中・再接続完了）はうさぎを無言に保つ。
        assert!(abnormal_daemon_speech(None).is_none());
        assert!(
            abnormal_daemon_speech(Some(&Feedback::Progress(SafeMessage::new("creating"))))
                .is_none()
        );
        assert!(abnormal_daemon_speech(Some(&Feedback::Reconnected)).is_none());

        // 切断・再同期は接続状態の異常として吹き出しに出す。
        assert!(bubble(&Feedback::Disconnected).contains("daemon 切断"));
        assert!(bubble(&Feedback::ResyncRequired).contains("再同期が必要"));

        // 操作/端末エラーは安全な message を 2 行目に載せ、error ID は載せない。
        let op = bubble(&Feedback::OperationError(SafeError {
            message: SafeMessage::new("Session creation failed"),
            error_id: "err-safe-7".to_owned(),
        }));
        assert!(op.contains("操作エラー"));
        assert!(op.contains("Session creation failed"));
        assert!(!op.contains("err-safe-7"));

        let term = bubble(&Feedback::TerminalError(SafeError {
            message: SafeMessage::new("Could not attach terminal"),
            error_id: "err-safe-9".to_owned(),
        }));
        assert!(term.contains("端末エラー"));
        assert!(term.contains("Could not attach terminal"));
    }

    #[test]
    fn home_bubble_surfaces_a_disconnected_daemon_and_stays_silent_when_healthy() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::Disconnected)),
        );
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[]);
        let text = strip(&render_home(30, 80, &home).join("\n"));
        // 切断はうさぎの上に tail 付きの吹き出しとして現れる。
        assert!(text.contains("╰──┬"), "abnormal state opens a bubble");
        assert!(text.contains("daemon 切断"));

        // 進行中は正常系なので吹き出しを出さず、無言のうさぎだけが残る。
        let mut healthy = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut healthy,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::Progress(
                SafeMessage::new("creating"),
            ))),
        );
        let home = HomeProjection::from_state(&healthy, "work", Path::new("/work"), &[]);
        let text = strip(&render_home(30, 80, &home).join("\n"));
        assert!(!text.contains("╰──┬"), "healthy state stays silent");
        assert!(text.contains("(o.o)?"));
    }

    #[test]
    fn explicit_mascot_speech_wins_over_the_daemon_state_bubble() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::Disconnected)),
        );
        let speech = MascotSpeech::new(["同期済み".to_owned()]).expect("speech");
        let home = HomeProjection::from_state(&state, "work", Path::new("/work"), &[])
            .with_mascot_speech(Some(speech));
        let text = strip(&render_home(30, 80, &home).join("\n"));
        assert!(text.contains("同期済み"));
        assert!(!text.contains("daemon 切断"));
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
            Path::new("/work"),
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
    fn switch_names_the_hovered_session_in_the_right_pane_and_closeup_names_the_target() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let sessions = [
            projected_session(first, "first", "/work/first"),
            projected_session(second, "second", "/work/second"),
        ];
        let mut state = AppState::home(workspace, vec![first, second]);
        // Activate the first session, then return to Switch and hover the second.
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlO));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(state.active(), Some(first));

        let switch = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions);
        let frame = render_home(18, 100, &switch);
        let right = |frame: &[String], row: usize| {
            strip(&frame[row])
                .split_once('│')
                .expect("pane divider")
                .1
                .to_owned()
        };
        // The right pane follows the cursor, not the target Closeup would act on.
        assert!(right(&frame, CHROME_ROWS).contains("second"));
        assert!(!right(&frame, CHROME_ROWS).contains("first"));
        // …and says so, because the hovered pane is not yet the command target.
        assert!(
            frame
                .iter()
                .any(|line| strip(line).contains("preview pane"))
        );
        assert!(!frame.iter().any(|line| strip(line).contains("active pane")));

        // Director is a foreground drawer, not a change of the Switch preview
        // target. The pane behind it must keep the hovered session's identity
        // and phase instead of falling back to the active session (or absent).
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: runtime_ref(workspace, second),
                phase: AgentPhase::Running,
            }),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
        let director = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions);
        assert_eq!(director.preview, Some(second));
        assert_eq!(director.preview_phase, TargetPhase::Running);
        assert_eq!(director.preview_label(), "second");
        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));

        // Entering Closeup makes the hovered session the target; the pane header
        // is unchanged, and the footer now names an active pane.
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.active(), Some(second));
        let closeup = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions);
        let frame = render_home(26, 100, &closeup);
        assert!(right(&frame, CHROME_ROWS).contains("second"));
        assert!(frame.iter().any(|line| strip(line).contains("active pane")));
    }

    /// The right pane is bright only while it owns input: a Closeup route whose
    /// selected tab is a live terminal, with nothing in front of it. Switch, a
    /// pending tab, and an open Director drawer all leave its controls inert, so
    /// each keeps the pane dim.
    #[test]
    fn home_right_pane_is_bright_only_while_a_live_tab_owns_input() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let target = Target::Session(session);
        let terminal = terminal_ref(workspace, session);
        let mut pane = PaneState::new(PaneSelection::Target(target));
        let _ = reduce(
            &mut pane,
            PaneEvent::Request {
                operation,
                target,
                kind: PaneKind::Agent,
            },
        );
        let state = AppState::home(workspace, vec![session]);
        let sessions = [projected_session(session, "session", "/work/session")];
        let right_pane_of = |home: &HomeProjection| {
            render_home(18, 100, home)[CHROME_ROWS]
                .split_once('│')
                .expect("pane divider")
                .1
                .to_owned()
        };
        let live_view = || {
            Some(TerminalViewProjection {
                total_rows: 1,
                rows: vec!["live row".to_owned()],
                row_offset: 0,
                scroll: 0,
                feedback: None,
            })
        };

        let switch = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions)
            .with_pane(&pane);
        let switch_right = right_pane_of(&switch);
        assert!(switch_right.contains("\u{1b}[2m"));
        assert!(switch_right.contains("\u{1b}[2;36msession"));
        assert!(!switch_right.contains("\u{1b}[1;36m"));

        let mut state = state;
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        // The pending tab steps the auto-opened action launcher aside, so the
        // pane surface itself is what the frame draws.
        let _ = update(
            &mut state,
            AppEvent::PaneTabAvailability {
                available: true,
                error: None,
            },
        );
        // Closeup without a live viewport (the pending Agent tab) stays dim: the
        // tab owns no PTY input yet.
        let pending = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions)
            .with_pane(&pane);
        assert!(right_pane_of(&pending).contains("\u{1b}[2;36msession"));

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
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let closeup = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions)
            .with_pane(&pane)
            .with_terminal_view(live_view());
        let closeup_right = right_pane_of(&closeup);
        assert!(closeup_right.contains("\u{1b}[1;36msession"));
        assert!(!closeup_right.starts_with("\u{1b}[2m"));

        // The Director drawer owns input while it is open, so the managed pane
        // behind it is dim in its own right — not only through the drawer's
        // background dimming.
        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleDirectorDrawer));
        let drawer = HomeProjection::from_state(&state, "work", Path::new("/work"), &sessions)
            .with_pane(&pane)
            .with_terminal_view(live_view());
        assert!(!drawer.right_pane_focused());
    }

    #[test]
    fn inactive_right_pane_removes_terminal_focus_emphasis_and_cursor() {
        use crate::presentation::frame::TERMINAL_CURSOR_MARKER;

        let focused = format!("\u{1b}[7;48;5;240m{TERMINAL_CURSOR_MARKER}focused\u{1b}[22m tail");
        let dimmed = super::dim_inactive_right_pane(true, vec![focused]);

        assert_eq!(strip(&dimmed[0]), "focused tail");
        assert!(!dimmed[0].contains(TERMINAL_CURSOR_MARKER));
        assert!(!dimmed[0].contains("[7"));
        assert!(!dimmed[0].contains("48;5;240"));
        assert!(!dimmed[0].contains("[2;22m"));
        assert!(dimmed[0].contains("\u{1b}[2m"));
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
                Path::new("/work"),
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
                Path::new("/work"),
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
            Path::new("/work"),
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane);

        let text = joined_home(&home);
        assert!(text.contains("feedback: agent launch is unavailable"));
        assert!(text.contains("No tabs stirring yet. Enter starts one."));
    }

    #[test]
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
        let home =
            HomeProjection::from_state(&state, "work", Path::new("/work"), &[]).with_pane(&pane);
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

    #[test]
    fn sidebar_metadata_keeps_the_pr_count_at_the_right_edge() {
        let diff = GitDiff {
            base: "origin/main".to_owned(),
            ahead: 1,
            behind: 2,
            added: 188,
            removed: 5,
        };
        let columns = SidebarDiffColumns {
            ahead: 1,
            behind: 1,
            added: 3,
            removed: 1,
        };
        let badge = format!("{PR_ICON} 2");
        let rendered = sidebar_metadata(
            "| 2h ago",
            Some(&diff),
            columns,
            Some(&badge),
            PR_RESERVE_WIDTH,
            32,
            false,
        );
        let plain = strip(&rendered);

        assert_eq!(display_width(&rendered), 32);
        assert!(plain.contains("↑1 ↓2 + 188 - 5"));
        assert!(plain.ends_with(&badge));
        assert!(!plain.contains("PR #"));
    }

    #[test]
    fn sidebar_metadata_prioritizes_the_pr_badge_when_too_narrow() {
        let badge = format!("{PR_ICON} 2");
        let rendered = sidebar_metadata(
            "| 2h ago",
            Some(&GitDiff {
                base: "origin/main".to_owned(),
                ahead: 1,
                behind: 2,
                added: 188,
                removed: 5,
            }),
            SidebarDiffColumns {
                ahead: 1,
                behind: 1,
                added: 3,
                removed: 1,
            },
            Some(&badge),
            PR_RESERVE_WIDTH,
            PR_RESERVE_WIDTH,
            false,
        );

        assert_eq!(display_width(&rendered), PR_RESERVE_WIDTH);
        assert_eq!(strip(&rendered), badge);
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
            Path::new("/tmp/actual"),
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
            Path::new("/work"),
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
            Path::new("/tmp/actual"),
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane)
        .with_terminal_view(Some(TerminalViewProjection {
            total_rows: view_rows.len(),
            rows: view_rows,
            row_offset: 0,
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
            Path::new("/tmp/actual"),
            &[projected_session(session, "session", "/work/session")],
        )
        .with_pane(&pane)
        .with_terminal_view(Some(TerminalViewProjection {
            total_rows: rows.len(),
            rows,
            row_offset: 0,
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
            Path::new("/work"),
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

    #[test]
    fn load_style_escalates_colour_at_the_busy_and_hot_thresholds() {
        // Below `busy` the metric stays calm: an explicit dim white so it does not
        // inherit the pink mascot foreground.
        assert_eq!(
            load_style(2_999, 3_000, 12_000),
            Style::new().fg(Color::White).dim()
        );
        // At (and above) `busy` but below `hot` it warns in yellow.
        assert_eq!(load_style(3_000, 3_000, 12_000), Role::Warning.style());
        assert_eq!(load_style(11_999, 3_000, 12_000), Role::Warning.style());
        // At (and above) `hot` it turns red.
        assert_eq!(load_style(12_000, 3_000, 12_000), Role::Danger.style());
    }

    #[test]
    fn format_memory_switches_from_mebibytes_to_gibibytes() {
        // Below one gibibyte the footprint reads in whole mebibytes.
        assert_eq!(format_memory(45 * MEBIBYTE), "45MB");
        // At a whole gibibyte the tenths digit is zero.
        assert_eq!(format_memory(2 * GIBIBYTE), "2.0GB");
        // A fractional gibibyte renders a single tenths digit.
        assert_eq!(format_memory(GIBIBYTE + 5 * 107_374_183), "1.5GB");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One projection fixture keeps tab, notice, and removal state consistent.
    fn production_projection_contract_covers_tabs_notices_removal_and_accessors() {
        use usagi_core::domain::agent::CallerRef;
        use usagi_core::domain::id::{AgentId, UserDecisionId};
        use usagi_core::domain::user_decision::{
            UserDecision, UserDecisionOwner, UserDecisionStatus,
        };

        let mut view = workspace();
        assert_eq!(view.name(), "actual");
        assert_eq!(view.path(), std::path::Path::new("/tmp/actual"));
        let replacement = vec![session("only", None, SessionOrigin::Human)];
        view.replace_sessions(replacement.clone());
        assert_eq!(view.sessions(), replacement);

        let mut one = session("one", None, SessionOrigin::Human);
        one.prs.push(PrLink::new(1, "https://example.test/pull/1"));
        assert_eq!(
            ProjectedSession::from_record(SessionId::new(), &one)
                .pr_summary
                .as_deref(),
            Some(format!("{PR_ICON} 1").as_str())
        );
        one.prs.push(PrLink::new(2, "https://example.test/pull/2"));
        assert_eq!(
            ProjectedSession::from_record(SessionId::new(), &one)
                .pr_summary
                .as_deref(),
            Some(format!("{PR_ICON} 2").as_str())
        );

        let target = Target::Root(WorkspaceId::new());
        let operation = OperationId::new();
        let pending = crate::usecase::application::pane::PendingPane {
            operation,
            target,
            kind: PaneKind::Terminal,
        };
        for kind in [PaneKind::Terminal, PaneKind::Agent, PaneKind::Diff] {
            let mut item = pending;
            item.kind = kind;
            let tab = PaneTab::Pending(item);
            assert!(!pane_tab_label(&tab).is_empty());
            let ready = PaneTab::Ready(item);
            assert!(!pane_tab_label(&ready).is_empty());
            assert!(pane_tab_selected(
                &ready,
                &PaneSelection::Tab(TabSelection::Ready(operation))
            ));
        }
        let terminal = TerminalRef {
            workspace_id: WorkspaceId::new(),
            worktree_id: WorktreeId::new(),
            session_id: None,
            terminal_id: TerminalId::new(),
            daemon_generation: DaemonGeneration::new(),
        };
        for kind in [PaneKind::Terminal, PaneKind::Agent, PaneKind::Diff] {
            assert!(
                !pane_tab_label(&PaneTab::Live(
                    crate::usecase::application::pane::LivePane {
                        terminal: terminal.clone(),
                        kind,
                    }
                ))
                .is_empty()
            );
        }

        for phase in [
            TargetPhase::Absent,
            TargetPhase::Ready,
            TargetPhase::Running,
            TargetPhase::Waiting,
            TargetPhase::Done,
        ] {
            assert!(!phase_label(phase).is_empty());
        }
        for feedback in [
            Feedback::Progress(SafeMessage::new("working")),
            Feedback::OperationError(SafeError {
                message: SafeMessage::new("operation failed"),
                error_id: "err-operation".to_owned(),
            }),
            Feedback::TerminalError(SafeError {
                message: SafeMessage::new("terminal failed"),
                error_id: "err-terminal".to_owned(),
            }),
            Feedback::Disconnected,
            Feedback::Reconnected,
            Feedback::ResyncRequired,
        ] {
            assert!(!feedback_label(Some(&feedback)).is_empty());
        }

        let workspace_id = WorkspaceId::new();
        let decision_id = UserDecisionId::new();
        let decision = UserDecision {
            decision_id,
            owner: UserDecisionOwner {
                workspace_id,
                session_id: None,
                caller: CallerRef {
                    session_id: None,
                    agent_id: AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "Deploy?".into(),
            prompt: "Proceed?".into(),
            options: Vec::new(),
            allow_freeform: true,
            expires_at: None,
            idempotency_key: None,
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: now(),
            resolved_at: None,
        };
        let mut state = AppState::home(workspace_id, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace: workspace_id,
                decisions: vec![decision],
            }),
        );
        let home = HomeProjection::from_state(&state, "actual", Path::new("/tmp/actual"), &[]);
        let frame = super::render_home(20, 80, &home).join("\n");
        assert!(strip(&frame).contains("Deploy?"));

        let session_id = SessionId::new();
        let mut removing = projected_session(session_id, "removing", "/tmp/removing");
        removing.removing = true;
        let mut state = AppState::home(workspace_id, vec![session_id]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let home =
            HomeProjection::from_state(&state, "actual", Path::new("/tmp/actual"), &[removing]);
        assert!(strip(&super::render_home(20, 80, &home).join("\n")).contains("removing"));
    }

    /// #510: an interrupted history tab is labelled and explained only from the
    /// closed provider/reason vocabulary, and its body replaces the phase line.
    #[test]
    #[allow(clippy::too_many_lines)] // Label, selection, and every body variant.
    fn interrupted_history_tabs_render_safe_labels_and_a_resume_hint() {
        use crate::usecase::application::interrupted_tab::InterruptedTab;
        use crate::usecase::application::pane::{InterruptedPane, PaneEvent, PaneState, reduce};
        use usagi_core::domain::agent::{
            AgentResumeTarget, ProviderKind, ProviderResumePhase, ProviderResumeReason,
        };
        use usagi_core::domain::id::{AgentContinuationRef, AgentResumeSourceId, AgentRuntimeId};

        let workspace = WorkspaceId::new();
        let target = Target::Root(workspace);
        let terminal = TerminalRef {
            workspace_id: workspace,
            worktree_id: WorktreeId::new(),
            session_id: None,
            terminal_id: TerminalId::new(),
            daemon_generation: DaemonGeneration::new(),
        };
        let continuation = AgentContinuationRef::new();
        let resumable = InterruptedTab {
            continuation,
            session_id: None,
            last_terminal: terminal.clone(),
            provider: Some(ProviderKind::Claude),
            last_known_phase: Some(ProviderResumePhase::Interrupted),
            reason: ProviderResumeReason::ExplicitResumeAvailable,
            target: Some(AgentResumeTarget {
                continuation,
                source: AgentResumeSourceId::new(),
                workspace_id: workspace,
                session_id: None,
                worktree_id: terminal.worktree_id,
                runtime_id: AgentRuntimeId::new(),
                adapter_revision: 3,
            }),
        };
        let unresumable = InterruptedTab {
            continuation: AgentContinuationRef::new(),
            provider: None,
            reason: ProviderResumeReason::ProviderMetadataUnavailable,
            target: None,
            ..resumable.clone()
        };

        assert_eq!(
            pane_tab_label(&PaneTab::Interrupted(InterruptedPane {
                tab: resumable.clone(),
                resuming: None,
            })),
            "Claude (interrupted)"
        );
        assert_eq!(
            pane_tab_label(&PaneTab::Interrupted(InterruptedPane {
                tab: resumable.clone(),
                resuming: Some(OperationId::new()),
            })),
            "Claude (resuming)"
        );
        // A lineage that kept no provider metadata stays neutral.
        assert_eq!(
            pane_tab_label(&PaneTab::Interrupted(InterruptedPane {
                tab: unresumable.clone(),
                resuming: None,
            })),
            "Agent (interrupted)"
        );
        assert!(pane_tab_selected(
            &PaneTab::Interrupted(InterruptedPane {
                tab: resumable.clone(),
                resuming: None,
            }),
            &PaneSelection::Tab(TabSelection::Interrupted(continuation)),
        ));
        assert!(!pane_tab_selected(
            &PaneTab::Interrupted(InterruptedPane {
                tab: resumable.clone(),
                resuming: None,
            }),
            &PaneSelection::Target(target),
        ));

        // The selected tab's body states the explicit action; the unresumable
        // one states only its safe reason.
        let mut pane = PaneState::new(PaneSelection::Target(target));
        let _ = reduce(
            &mut pane,
            PaneEvent::RestoreInterrupted {
                tabs: vec![resumable.clone(), unresumable.clone()],
            },
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Interrupted(continuation))),
        );
        // Render on the Closeup surface, where the tab strip lives.
        let session = SessionId::new();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let sessions = [projected_session(session, "session", "/work/session")];
        let home = HomeProjection::from_state(&state, "repo", Path::new("/repo"), &sessions)
            .with_pane(&pane);
        let detail = home.pane_detail.clone().unwrap();
        assert!(detail.contains("Ctrl-O r"), "{detail}");
        let frame = render_home(24, 160, &home).join("\n");
        assert!(frame.contains("Claude (interrupted)"), "{frame}");
        assert!(frame.contains("Ctrl-O r"), "{frame}");
        assert!(
            !frame.contains(&resumable.continuation.as_str()),
            "a raw lineage identifier must never reach the frame"
        );

        let _ = reduce(
            &mut pane,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Interrupted(
                unresumable.continuation,
            ))),
        );
        let detail = HomeProjection::from_state(&state, "repo", Path::new("/repo"), &sessions)
            .with_pane(&pane)
            .pane_detail
            .clone()
            .unwrap();
        assert_eq!(detail, unresumable.safe_detail());

        // While the resume is in flight the body says so.
        let _ = reduce(
            &mut pane,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Interrupted(continuation))),
        );
        let _ = reduce(
            &mut pane,
            PaneEvent::ResumeStarted {
                continuation,
                operation: OperationId::new(),
            },
        );
        let detail = HomeProjection::from_state(&state, "repo", Path::new("/repo"), &sessions)
            .with_pane(&pane)
            .pane_detail
            .clone()
            .unwrap();
        assert!(detail.contains("resuming"), "{detail}");
    }

    #[test]
    fn rootless_sidebar_handles_rows_that_do_not_fit_and_stale_root_projection() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let state = AppState::home(workspace, vec![session]);
        let sessions = [projected_session(session, "session", "/work/session")];
        let home = HomeProjection::from_state(&state, "repo", Path::new("/repo"), &sessions);

        // One content line cannot fit the first two-line session row; the footer
        // still occupies the final line without partial-row rendering.
        let pane = home_left_pane(2, LEFT_WIDTH, &home, now());
        assert_eq!(pane.len(), 2);
        assert!(!pane.iter().any(|line| strip(line).contains("session")));

        let rows = home.rows();
        assert_eq!(home_viewport_start(LEFT_WIDTH, &home, &rows, 1, 1), 1);

        // Root is no longer a row, but a stale synthetic projection stays total
        // and cannot reveal a hidden `main` action.
        let stale_lines = home_row_lines_at(
            LEFT_WIDTH,
            &home,
            Selection::Target(Target::Root(workspace)),
            SidebarDiffColumns::default(),
            PR_RESERVE_WIDTH,
            now(),
        );
        assert_eq!(stale_lines.len(), 1);
        assert!(!strip(&stale_lines[0]).contains("main"));
    }
}

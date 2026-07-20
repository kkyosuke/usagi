//! Home の純粋な application controller。
//!
//! [`update`] は TUI-local の [`AppEvent`] を受け、状態を更新して外部へ依頼する
//! [`Effect`] を返す。daemon の wire 型はここへ持ち込まない。実行側は
//! [`BackendPort`] で effect を backend 固有の command に変換し、テストでは
//! [`FakeBackend`] の command log と event queue を使う。

use std::collections::VecDeque;
use std::path::PathBuf;

use usagi_core::domain::agent::{AgentProfileId, ModelSelector};
use usagi_core::domain::id::{
    AgentRuntimeRef, OperationId, SessionId, UserDecisionId, WorkspaceId,
};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::pullrequest::PrLink;
use usagi_core::domain::session_lifecycle::AgentPhase;
use usagi_core::domain::user_decision::{UserDecision, UserDecisionAnswer, UserDecisionStatus};

use crate::usecase::terminal_input::{KeyCode, KeyEventKind, LiveInput, RuntimeEvent};
use crate::usecase::{closeup, overview};

/// Home の常駐 route。これ以外の常駐 mode は作らない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeMode {
    /// 一覧を移動し、実行対象を選ぶ mode。
    Switch,
    /// active target の pane を操作する mode。
    Closeup,
}

/// application の常駐 route。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Home。常駐 route はこの 1 つで、mode は [`HomeMode`] の二択である。
    Home(HomeMode),
}

/// Home の一時的な重ね表示。常駐 mode には数えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// workspace scope の command surface。
    Overview,
    /// active target scope の action surface。
    Closeup,
    /// Detach confirmation. This is TUI-local: confirming it never stops a
    /// daemon-owned terminal or operation.
    QuitConfirmation,
    /// active target の note / todo / decision scratchpad。
    Notes,
    /// workspace または session の environment editor。
    Environment,
    /// Home 左ペインの `+ new session` に対する入力。常駐 route ではない。
    CreateSession,
    /// Workspace-scoped pending user decisions and their answer editor.
    Decisions,
    /// active target scope の Pull Request 一覧。素材は port から還流する。
    Prs,
    /// active target の Markdown preview。素材は port から還流する。
    Preview,
    /// session 作成が accept 後に失敗したことを伝える dialog。表示は safe message だけ。
    CreateSessionError,
}

/// session name に許される最大文字数（表示・path 双方の実害を避ける上限）。
const MAX_SESSION_NAME_LEN: usize = 64;

/// daemon へ送る前の、TUI-local な新規 session 入力。
///
/// 左サイドバーの `+ new session` 行に inline 展開する name-only 入力。profile/model
/// は指定せず、daemon の workspace default policy に委ねる。
/// `existing` は現在表示中の session name で、同名入力を daemon へ送る前に local で弾く。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CreateSessionForm {
    name: String,
    error: Option<Notice>,
    existing: Vec<String>,
}

impl CreateSessionForm {
    /// 表示中 session の name を与えて空の form を作る。同名検出に使う。
    #[must_use]
    pub fn new(existing: Vec<String>) -> Self {
        Self {
            existing,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn error(&self) -> Option<&Notice> {
        self.error.as_ref()
    }

    fn push(&mut self, character: char) {
        self.name.push(character);
        self.revalidate();
    }

    fn backspace(&mut self) {
        self.name.pop();
        self.revalidate();
    }

    /// 入力のたびに name の live validation を反映する。空名は「入力途中」であって
    /// error にはせず（submit 時にだけ弾く）、不正文字・64 文字超過・同名は即座に
    /// 行の下の error として見せる。draft は決して失わない。
    fn revalidate(&mut self) {
        self.error = validate_session_name_live(&self.name, &self.existing);
    }

    /// submit（Enter）時の検証。空名はここで初めて error になる。
    fn request(&mut self) -> Result<SessionCreateIntent, Notice> {
        // 空名は submit 時にだけ弾く（入力途中は error にしない）。非空の name は
        // 不正文字・64 文字超過・同名を local validation で拒否する。
        let name = required_create_value(&self.name, "session name is required")?;
        if let Some(error) = validate_session_name_live(&self.name, &self.existing) {
            return Err(error);
        }
        Ok(SessionCreateIntent {
            name,
            profile: None,
            model: None,
        })
    }
}

/// 入力途中でも判定できる name の validation。空名（入力途中）は `None` を返し、
/// 不正文字・64 文字超過・表示中 session との同名だけを safe な error にする。
/// 返す message は利用者の入力を復唱せず、内部詳細も含めない。
fn validate_session_name_live(name: &str, existing: &[String]) -> Option<Notice> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Messages are short on purpose: they render inline in the 36-column sidebar
    // row beside the typed name, so a long sentence would just be clipped.
    if !trimmed
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        return Some(Notice::new("invalid character"));
    }
    if trimmed.chars().count() > MAX_SESSION_NAME_LEN {
        return Some(Notice::new("name too long (max 64)"));
    }
    if existing.iter().any(|current| current == trimmed) {
        return Some(Notice::new("name already exists"));
    }
    None
}

/// Validated new-session request. This is intentionally product-neutral: adapter
/// specific CLI flags and model allowlists remain daemon adapter concerns. profile
/// / model は現状 TUI の作成フローからは指定せず（常に `None`、daemon の workspace
/// default policy に委ねる）、型は将来の daemon 側選択のため `Option` を保つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCreateIntent {
    pub name: String,
    pub profile: Option<AgentProfileId>,
    pub model: Option<ModelSelector>,
}

fn required_create_value(value: &str, message: &str) -> Result<String, Notice> {
    let value = value.trim();
    (!value.is_empty())
        .then(|| value.to_owned())
        .ok_or_else(|| Notice::new(message))
}

fn optional_profile(value: &str) -> Result<Option<AgentProfileId>, Notice> {
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        AgentProfileId::new(value)
            .map(Some)
            .map_err(|_| Notice::new("invalid agent profile"))
    }
}

/// Note editor で現在表示・編集している section。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSection {
    Note,
    Todos,
    Decisions,
}

/// Target-local scratchpad の overlay state。
///
/// 保存前の値も含め TUI が所有する。port の失敗は [`error`](Self::error) にだけ
/// 投影するので、利用者が入力した内容は失われない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEditor {
    target: Target,
    scratchpad: Scratchpad,
    section: NoteSection,
    draft: String,
    error: Option<SafeError>,
}

impl NoteEditor {
    #[coverage(off)]
    fn loading(target: Target) -> Self {
        Self {
            target,
            scratchpad: Scratchpad::default(),
            section: NoteSection::Note,
            draft: String::new(),
            error: None,
        }
    }

    /// Overlay が対象とする stable identity。
    #[must_use]
    #[coverage(off)]
    pub const fn target(&self) -> Target {
        self.target
    }
    /// 現在の表示・編集値。
    #[must_use]
    #[coverage(off)]
    pub fn scratchpad(&self) -> &Scratchpad {
        &self.scratchpad
    }
    /// 選択された section。
    #[must_use]
    #[coverage(off)]
    pub const fn section(&self) -> NoteSection {
        self.section
    }
    /// todo / decision 追加用、または note の編集値。
    #[must_use]
    #[coverage(off)]
    pub fn draft(&self) -> &str {
        &self.draft
    }
    /// port が分類した安全なエラー。
    #[must_use]
    #[coverage(off)]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
}

/// One editable environment variable. Values intentionally remain inside the
/// settings port and TUI-local state; they are never placed in a notice/error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentEntry {
    pub name: String,
    pub value: String,
}

/// workspace / session environment editor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentEditor {
    target: Target,
    entries: Vec<EnvironmentEntry>,
    error: Option<SafeError>,
}

/// Local navigation and draft state for a durable user decision.  The durable
/// record itself remains daemon-owned; dismissing this state never mutates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionEditor {
    decision: UserDecision,
    selected_option: usize,
    freeform: String,
    error: Option<SafeError>,
}

impl DecisionEditor {
    fn new(decision: UserDecision) -> Self {
        Self {
            decision,
            selected_option: 0,
            freeform: String::new(),
            error: None,
        }
    }
    #[must_use]
    #[coverage(off)]
    pub fn decision(&self) -> &UserDecision {
        &self.decision
    }
    #[must_use]
    #[coverage(off)]
    pub const fn selected_option(&self) -> usize {
        self.selected_option
    }
    #[must_use]
    #[coverage(off)]
    pub fn freeform(&self) -> &str {
        &self.freeform
    }
    #[must_use]
    #[coverage(off)]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
}

/// The decisions overlay is either its persistent pending list or one answer
/// editor.  Returning from the editor keeps the list available for re-display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOverlayState {
    selected: usize,
    editor: Option<DecisionEditor>,
}

impl DecisionOverlayState {
    #[must_use]
    #[coverage(off)]
    pub const fn selected(&self) -> usize {
        self.selected
    }
    #[must_use]
    #[coverage(off)]
    pub fn editor(&self) -> Option<&DecisionEditor> {
        self.editor.as_ref()
    }
}

/// active target の Pull Request 一覧 overlay state。
///
/// 一覧 [`PrLink`] は domain データで、素材は port（[`Effect::LoadPullRequests`]）から
/// [`BackendEvent::PullRequestsLoaded`] として還流する。reducer が所有するのは選択位置と
/// 表示可能なエラーだけで、URL の妥当性検証や browser 起動は executor 側に残す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrOverlay {
    target: Target,
    prs: Vec<PrLink>,
    selected: usize,
    error: Option<SafeError>,
}

impl PrOverlay {
    #[coverage(off)]
    fn loading(target: Target) -> Self {
        Self {
            target,
            prs: Vec::new(),
            selected: 0,
            error: None,
        }
    }

    /// Overlay が対象とする stable identity。
    #[must_use]
    #[coverage(off)]
    pub const fn target(&self) -> Target {
        self.target
    }
    /// 表示中の PR 一覧。素材未着なら空。
    #[must_use]
    #[coverage(off)]
    pub fn prs(&self) -> &[PrLink] {
        &self.prs
    }
    /// 選択中の添字。
    #[must_use]
    #[coverage(off)]
    pub const fn selected(&self) -> usize {
        self.selected
    }
    /// 選択中の PR。一覧が空なら `None`。
    #[must_use]
    #[coverage(off)]
    pub fn selected_pr(&self) -> Option<&PrLink> {
        self.prs.get(self.selected)
    }
    /// port が分類した安全なエラー。
    #[must_use]
    #[coverage(off)]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
}

/// active target の Markdown preview overlay state。
///
/// 表示行は port（[`Effect::LoadPreview`]）から [`BackendEvent::PreviewLoaded`] として
/// 還流する安全な文字列で、reducer が所有するのは scroll 位置と表示可能なエラーだけである。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewOverlay {
    target: Target,
    lines: Vec<String>,
    scroll: usize,
    error: Option<SafeError>,
}

impl PreviewOverlay {
    #[coverage(off)]
    fn loading(target: Target) -> Self {
        Self {
            target,
            lines: Vec::new(),
            scroll: 0,
            error: None,
        }
    }

    /// Overlay が対象とする stable identity。
    #[must_use]
    #[coverage(off)]
    pub const fn target(&self) -> Target {
        self.target
    }
    /// 表示可能な preview 行。素材未着なら空。
    #[must_use]
    #[coverage(off)]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }
    /// 現在の先頭行 offset。
    #[must_use]
    #[coverage(off)]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }
    /// port が分類した安全なエラー。
    #[must_use]
    #[coverage(off)]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
}

impl EnvironmentEditor {
    #[coverage(off)]
    fn loading(target: Target) -> Self {
        Self {
            target,
            entries: Vec::new(),
            error: None,
        }
    }

    #[must_use]
    #[coverage(off)]
    pub const fn target(&self) -> Target {
        self.target
    }
    #[must_use]
    #[coverage(off)]
    pub fn entries(&self) -> &[EnvironmentEntry] {
        &self.entries
    }
    #[must_use]
    #[coverage(off)]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
}

/// daemon wire と独立した TUI の target projection。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// workspace root。
    Root(WorkspaceId),
    /// stable session identity で追跡する session。
    Session(SessionId),
}

impl Target {
    /// Returns the owning session, or `None` for the workspace root. This is the
    /// scope discriminator threaded through the daemon launch vocabulary.
    #[must_use]
    pub fn session_id(self) -> Option<SessionId> {
        match self {
            Self::Root(_) => None,
            Self::Session(session) => Some(session),
        }
    }
}

/// Home の navigation cursor。`NewSession` は action row であり active にはならない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// root または session target を選ぶ。
    Target(Target),
    /// `+ new session` action row を選ぶ。
    NewSession,
}

/// 非同期操作を reducer が追跡する TUI-local token。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PendingToken(u64);

impl PendingToken {
    /// テストや backend adapter が token の数値を確認する。
    #[must_use]
    #[coverage(off)]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Rebuild a token from its raw value. Only tests synthesize effects
    /// directly; the reducer is the sole producer at runtime.
    #[cfg(test)]
    #[must_use]
    #[coverage(off)]
    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

/// pending の操作種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// session create request。
    CreateSession,
}

/// 操作中表示と completion の対応付け。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingOperation {
    /// backend に渡す TUI-local token。
    pub token: PendingToken,
    /// 実行中の操作。
    pub kind: PendingKind,
    /// daemon-authoritative durable operation identity.
    pub operation_id: OperationId,
    /// User interaction count at acceptance. Landing is safe only when unchanged.
    pub interaction_at_accept: u64,
}

/// 画面に安全に表示できる通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// 表示する安全な文言。
    pub message: String,
}

impl Notice {
    /// 表示用に検証済みの文言を作る。
    #[must_use]
    #[coverage(off)]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A phase projected for one Home target. `Done` folds daemon `ended` and
/// `exited` together because neither leaves an interactive Agent pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPhase {
    /// No known Agent runtime belongs to the target.
    Absent,
    /// All known runtimes are ready.
    Ready,
    /// At least one runtime is executing work.
    Running,
    /// At least one runtime is waiting for input.
    Waiting,
    /// At least one runtime has ended or exited.
    Done,
}

impl TargetPhase {
    #[coverage(off)]
    const fn rank(self) -> u8 {
        match self {
            Self::Absent => 0,
            Self::Ready => 1,
            Self::Running => 2,
            Self::Waiting => 3,
            Self::Done => 4,
        }
    }

    #[coverage(off)]
    fn from_agent_phase(phase: AgentPhase) -> Self {
        match phase {
            AgentPhase::Ready => Self::Ready,
            AgentPhase::Running => Self::Running,
            AgentPhase::Waiting => Self::Waiting,
            AgentPhase::Ended | AgentPhase::Exited => Self::Done,
        }
    }
}

/// One runtime-local phase entry. The complete runtime reference is retained so
/// an update for one pane can never overwrite another pane in the same session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePhase {
    pub runtime: AgentRuntimeRef,
    pub phase: TargetPhase,
}

/// A message which a backend adapter has explicitly classified as safe to show.
/// No raw protocol error or detail field is representable by this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeMessage(String);

impl SafeMessage {
    #[must_use]
    #[coverage(off)]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    #[must_use]
    #[coverage(off)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A safe error summary. `error_id` is the only diagnostic identifier retained
/// by the TUI-local projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeError {
    pub message: SafeMessage,
    pub error_id: String,
}

/// Feedback displayed in Home's fixed status area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feedback {
    Progress(SafeMessage),
    OperationError(SafeError),
    TerminalError(SafeError),
    Disconnected,
    /// The connection was restored; the next snapshot or replay can reconcile
    /// the visible state without requiring a key press.
    Reconnected,
    /// The daemon requested a snapshot replacement rather than applying a
    /// potentially incomplete replay.
    ResyncRequired,
}

/// controller が所有する application state。
// These bools are independent runtime flags (live-pane availability, forced
// action modal, Ctrl-C grace, quit-confirmation focus), not a combinable state
// machine, so a single enum would not model them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    route: Route,
    overlay: Option<Overlay>,
    note_editor: Option<NoteEditor>,
    environment_editor: Option<EnvironmentEditor>,
    decisions: Vec<UserDecision>,
    unread_decisions: std::collections::BTreeSet<UserDecisionId>,
    decision_overlay: Option<DecisionOverlayState>,
    pr_overlay: Option<PrOverlay>,
    preview_overlay: Option<PreviewOverlay>,
    create_session: Option<CreateSessionForm>,
    create_session_error: Option<Notice>,
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    /// 表示中 session の name。新規作成の同名 validation にだけ使う advisory copy で、
    /// authoritative な identity は [`sessions`](Self::sessions) が持つ。
    session_names: Vec<String>,
    selected: Selection,
    active: Target,
    notice: Option<Notice>,
    runtimes: Vec<RuntimePhase>,
    feedback: Option<Feedback>,
    pending: Vec<PendingOperation>,
    next_pending_token: u64,
    interaction_count: u64,
    mascot_tick: u64,
    size: Option<(u16, u16)>,
    /// Last session press eligible to become the first half of a double click.
    /// The controller owns this stable identity after hit-testing; the shell
    /// supplies only coordinates and a monotonic timestamp.
    pending_session_click: Option<(SessionId, std::time::Duration)>,
    has_live_pane: bool,
    closeup_action_forced: bool,
    ctrl_c_grace: bool,
    /// Focus of the quit confirmation's Yes/No buttons. `true` keeps Yes
    /// focused; opening the overlay resets it. The presentation layer projects
    /// this into a `ConfirmationModal`, keeping the shared widget out of this
    /// usecase-layer state.
    quit_confirm_selected: bool,
}

impl AppState {
    /// workspace root を selected / active にした Home を作る。
    #[must_use]
    #[coverage(off)]
    pub fn home(workspace: WorkspaceId, sessions: Vec<SessionId>) -> Self {
        let root = Target::Root(workspace);
        Self {
            route: Route::Home(HomeMode::Switch),
            overlay: None,
            note_editor: None,
            environment_editor: None,
            decisions: Vec::new(),
            unread_decisions: std::collections::BTreeSet::new(),
            decision_overlay: None,
            pr_overlay: None,
            preview_overlay: None,
            create_session: None,
            create_session_error: None,
            workspace,
            sessions,
            session_names: Vec::new(),
            selected: Selection::Target(root),
            active: root,
            notice: None,
            runtimes: Vec::new(),
            feedback: None,
            pending: Vec::new(),
            next_pending_token: 1,
            interaction_count: 0,
            mascot_tick: 0,
            size: None,
            pending_session_click: None,
            has_live_pane: false,
            closeup_action_forced: false,
            ctrl_c_grace: false,
            quit_confirm_selected: true,
        }
    }

    /// 常駐 route。
    #[must_use]
    #[coverage(off)]
    pub const fn route(&self) -> Route {
        self.route
    }
    /// 最前面 overlay。閉じても [`route`](Self::route) は変わらない。
    #[must_use]
    #[coverage(off)]
    pub const fn overlay(&self) -> Option<Overlay> {
        self.overlay
    }
    /// Home sidebar mascot animation frame. Only [`AppEvent::Tick`] advances it.
    #[must_use]
    pub const fn mascot_tick(&self) -> u64 {
        self.mascot_tick
    }
    /// Open note editor, including unsaved values after a save failure.
    #[must_use]
    #[coverage(off)]
    pub fn note_editor(&self) -> Option<&NoteEditor> {
        self.note_editor.as_ref()
    }
    /// Open new-session form, including values retained after validation failure.
    #[must_use]
    #[coverage(off)]
    pub fn create_session_form(&self) -> Option<&CreateSessionForm> {
        self.create_session.as_ref()
    }
    /// Safe message for the create-failure dialog, present exactly while
    /// [`Overlay::CreateSessionError`] is open.
    #[must_use]
    #[coverage(off)]
    pub fn create_session_error(&self) -> Option<&Notice> {
        self.create_session_error.as_ref()
    }
    /// Open environment editor, including unsaved values after a save failure.
    #[must_use]
    #[coverage(off)]
    pub fn environment_editor(&self) -> Option<&EnvironmentEditor> {
        self.environment_editor.as_ref()
    }
    /// Pending decisions from the current workspace only.
    #[must_use]
    #[coverage(off)]
    pub fn decisions(&self) -> &[UserDecision] {
        &self.decisions
    }
    /// Pending decisions the user has not opened from the notice centre yet.
    #[must_use]
    #[coverage(off)]
    pub fn unread_decision_ids(&self) -> &std::collections::BTreeSet<UserDecisionId> {
        &self.unread_decisions
    }
    /// Open decision list/editor state, if its overlay is visible.
    #[must_use]
    #[coverage(off)]
    pub fn decision_overlay(&self) -> Option<&DecisionOverlayState> {
        self.decision_overlay.as_ref()
    }
    /// Open Pull Request overlay state, including its cursor and any safe error.
    #[must_use]
    #[coverage(off)]
    pub fn pr_overlay(&self) -> Option<&PrOverlay> {
        self.pr_overlay.as_ref()
    }
    /// Open Markdown preview overlay state, including its scroll and any safe error.
    #[must_use]
    #[coverage(off)]
    pub fn preview_overlay(&self) -> Option<&PreviewOverlay> {
        self.preview_overlay.as_ref()
    }
    /// navigation cursor。
    #[must_use]
    #[coverage(off)]
    pub const fn selected(&self) -> Selection {
        self.selected
    }
    /// command / Closeup の target。
    #[must_use]
    #[coverage(off)]
    pub const fn active(&self) -> Target {
        self.active
    }
    /// この Home が投影している workspace identity。
    #[must_use]
    #[coverage(off)]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }
    /// snapshot の stable session identity。
    #[must_use]
    #[coverage(off)]
    pub fn sessions(&self) -> &[SessionId] {
        &self.sessions
    }
    /// 表示中 session の name（同名 validation 用の advisory copy）。
    #[must_use]
    #[coverage(off)]
    pub fn session_names(&self) -> &[String] {
        &self.session_names
    }
    /// 最後の safe notice。
    #[must_use]
    #[coverage(off)]
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }
    /// 実行中操作。
    #[must_use]
    #[coverage(off)]
    pub fn pending(&self) -> &[PendingOperation] {
        &self.pending
    }
    /// Runtime phases retained for the current workspace only.
    #[must_use]
    #[coverage(off)]
    pub fn runtimes(&self) -> &[RuntimePhase] {
        &self.runtimes
    }
    /// The current safe feedback for the fixed Home feedback area.
    #[must_use]
    #[coverage(off)]
    pub fn feedback(&self) -> Option<&Feedback> {
        self.feedback.as_ref()
    }
    /// Aggregates phase for a target using `done > waiting > running > ready > absent`.
    #[must_use]
    #[coverage(off)]
    pub fn phase_for(&self, target: Target) -> TargetPhase {
        let scope = target.session_id();
        self.runtimes
            .iter()
            .filter(|entry| entry.runtime.session_id == scope)
            .map(|entry| entry.phase)
            .max_by_key(|phase| phase.rank())
            .unwrap_or(TargetPhase::Absent)
    }
    /// 最後に受け取った terminal geometry。
    #[must_use]
    #[coverage(off)]
    pub const fn size(&self) -> Option<(u16, u16)> {
        self.size
    }
    /// Whether the current Home projection has a live terminal or Agent pane.
    #[must_use]
    #[coverage(off)]
    pub const fn has_live_pane(&self) -> bool {
        self.has_live_pane
    }
    /// Monotonic count of user interactions (keys and live input) applied so far.
    ///
    /// A launch accepted at count `n` may only auto-focus its completed pane while
    /// this still reads `n`; a later key or input moves it and cancels the steal.
    /// This is the same gate the create-session flow uses via
    /// [`PendingOperation::interaction_at_accept`].
    #[must_use]
    #[coverage(off)]
    pub const fn interaction_count(&self) -> u64 {
        self.interaction_count
    }
    /// Whether the next management `Ctrl-C` is deliberately absorbed.
    #[must_use]
    #[coverage(off)]
    pub const fn ctrl_c_grace(&self) -> bool {
        self.ctrl_c_grace
    }
    /// Whether the quit confirmation currently focuses Yes. The presentation
    /// layer reads this to draw the shared Yes/No buttons in the right state.
    #[must_use]
    #[coverage(off)]
    pub const fn quit_confirm_selected(&self) -> bool {
        self.quit_confirm_selected
    }

    #[coverage(off)]
    fn root(&self) -> Target {
        Target::Root(self.workspace)
    }

    #[coverage(off)]
    fn rows(&self) -> Vec<Selection> {
        let mut rows = Vec::with_capacity(self.sessions.len() + 2);
        rows.push(Selection::Target(self.root()));
        rows.extend(
            self.sessions
                .iter()
                .copied()
                .map(|id| Selection::Target(Target::Session(id))),
        );
        rows.push(Selection::NewSession);
        rows
    }

    #[coverage(off)]
    fn move_selection(&mut self, direction: i8) {
        let rows = self.rows();
        let current = rows
            .iter()
            .position(|row| *row == self.selected)
            .unwrap_or(0);
        let next = if direction > 0 {
            (current + 1) % rows.len()
        } else {
            (current + rows.len() - 1) % rows.len()
        };
        self.selected = rows[next];
    }

    /// Move the cursor directly to `selection` when it names a live row.
    /// [`sidebar_selection_at`](Self::sidebar_selection_at) only ever resolves a
    /// row that exists, but the reducer stays defensive so a stale pointer event
    /// can never point the cursor at a session that has since disappeared.
    fn select_row(&mut self, selection: Selection) {
        if self.rows().contains(&selection) {
            self.selected = selection;
        }
    }

    /// Resolve a 0-based terminal cell to the Home sidebar row it lands on, using
    /// the same viewport geometry the frame is drawn with. Returns `None` for the
    /// header, the divider under Root, the mascot sidecar, the footer, or a click
    /// outside the sidebar body.
    ///
    /// This is the controller-owned hit-test the pointer reducer shares with the
    /// `home_left_pane` render: it mirrors the chrome rows, the left/right split,
    /// the foot-of-sidebar mascot reservation, and the scroll offset so a click
    /// always lands on the row the user sees. It reads the last terminal geometry
    /// from [`AppState::size`], so a pointer event before the first resize is
    /// inert.
    fn sidebar_selection_at(&self, column: u16, row: u16) -> Option<Selection> {
        let (raw_width, raw_height) = self.size?;
        let width = if raw_width == 0 {
            80
        } else {
            usize::from(raw_width)
        };
        let height = if raw_height == 0 {
            24
        } else {
            usize::from(raw_height)
        };
        let left = SIDEBAR_LEFT_WIDTH.min(width.saturating_sub(2));
        if usize::from(column) >= left
            || usize::from(row) < SIDEBAR_CHROME_ROWS
            || height <= SIDEBAR_CHROME_ROWS
        {
            return None;
        }
        let rows = self.rows();
        let body_height = height - SIDEBAR_CHROME_ROWS;
        if body_height == 1 {
            return (usize::from(row) == SIDEBAR_CHROME_ROWS).then(|| rows[0]);
        }
        let body_capacity = body_height - 1;
        let content_capacity =
            body_capacity.saturating_sub(sidebar_mascot_rows(left, body_capacity));
        let clicked = usize::from(row) - SIDEBAR_CHROME_ROWS;
        if clicked >= content_capacity {
            return None;
        }
        let selected_index = rows
            .iter()
            .position(|entry| *entry == self.selected)
            .unwrap_or(0);
        let start = sidebar_viewport_start(&rows, selected_index, content_capacity);
        let mut offset = 0;
        for entry in &rows[start..] {
            let lines = sidebar_row_content_lines(*entry);
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
    fn reconcile_selection(&mut self) {
        let rows = self.rows();
        if !rows.contains(&self.selected) {
            self.selected = Selection::Target(self.root());
        }
        if !self
            .sessions
            .iter()
            .any(|id| self.active == Target::Session(*id))
            && !matches!(self.active, Target::Root(_))
        {
            self.active = self.root();
        }
    }
}

/// Fixed chrome rows (title + spacer) above the Home sidebar's row body. Mirrors
/// `views::workspace`'s `CHROME_ROWS`; a compile-time assertion there keeps the
/// hit-test and the render agreeing on the geometry.
pub(crate) const SIDEBAR_CHROME_ROWS: usize = 2;
/// Desired Home sidebar left-pane width. The split clamps it to leave the right
/// pane at least one column. Mirrors `views::workspace`'s `LEFT_WIDTH`.
pub(crate) const SIDEBAR_LEFT_WIDTH: usize = 36;
/// Rows the foot-of-sidebar mascot reserves in the controller Home: three rabbit
/// lines and one trailing gap. The controller frame never renders a speech
/// bubble, so the reservation is constant whenever the mascot is shown.
pub(crate) const SIDEBAR_MASCOT_ROWS: usize = 4;
/// Minimum left-pane width that fits the mascot rabbit (its nine-cell art plus a
/// one-column indent). Below this the sidebar drops the mascot for list space.
pub(crate) const SIDEBAR_MASCOT_MIN_LEFT: usize = 10;

/// Rows the mascot sidecar reserves for a given left-pane width and body
/// capacity, mirroring `home_left_pane`'s reservation. The rabbit shows only
/// when the pane is wide enough for its art and the body has room for the
/// reservation plus two list rows.
fn sidebar_mascot_rows(left: usize, body_capacity: usize) -> usize {
    if left >= SIDEBAR_MASCOT_MIN_LEFT && body_capacity >= SIDEBAR_MASCOT_ROWS + 2 {
        SIDEBAR_MASCOT_ROWS
    } else {
        0
    }
}

/// Scroll rows the sidebar viewport uses to weight one row: Root spans its
/// identity line plus the divider beneath it, a session spans its identity and
/// metadata lines, and the `+ new session` action is a single line.
fn sidebar_row_height(row: Selection) -> usize {
    match row {
        Selection::Target(Target::Root(_) | Target::Session(_)) => 2,
        Selection::NewSession => 1,
    }
}

/// Body lines a row actually draws, excluding the divider `home_left_pane`
/// inserts after Root: a session identity row carries a metadata row, while Root
/// and the action row are single lines.
fn sidebar_row_content_lines(row: Selection) -> usize {
    match row {
        Selection::Target(Target::Session(_)) => 2,
        Selection::Target(Target::Root(_)) | Selection::NewSession => 1,
    }
}

/// First visible row so the selected row stays in view, mirroring the render's
/// scroll math: advance the viewport start until the selected row and everything
/// above it fits within `capacity`.
fn sidebar_viewport_start(rows: &[Selection], selected: usize, capacity: usize) -> usize {
    let mut start = 0;
    while start < selected
        && rows[start..=selected]
            .iter()
            .map(|row| sidebar_row_height(*row))
            .sum::<usize>()
            > capacity
    {
        start += 1;
    }
    start
}

/// terminal adapter が将来投影する入力語彙。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppKey {
    /// cursor を前の row へ動かす。
    Up,
    /// cursor を次の row へ動かす。
    Down,
    /// Move the focus left within a horizontal choice (the Yes/No confirmation).
    /// Outside such an overlay it is inert.
    Left,
    /// Move the focus right within a horizontal choice (the Yes/No confirmation).
    /// Outside such an overlay it is inert.
    Right,
    /// selected target を active にし Closeup を開く。
    Enter,
    /// Move to the next field in a local form.
    Tab,
    /// Delete the final character in the selected local form field.
    Backspace,
    /// Ctrl-A / terminals that decode it as Home while Home is in Switch mode.
    CtrlA,
    /// Ctrl-O returns Closeup to Switch. It has no effect while already in Switch.
    CtrlO,
    /// Ctrl-O n / Ctrl-N selects the next Closeup tab when a live pane owns input.
    CtrlN,
    /// Ctrl-O p / Ctrl-P selects the previous Closeup tab when a live pane owns input.
    CtrlP,
    /// Home navigation. Inside a create form it is deliberately inert: this
    /// string-only reducer has no byte cursor, and must never reopen the form.
    Home,
    /// 最前面の overlay を閉じる。Home の mode は変えない。
    Escape,
    /// Management-screen Ctrl-C. It is ignored in Switch mode; live Ctrl-C is
    /// classified before it reaches this reducer and is passed through to the PTY.
    CtrlC,
    /// Management-screen Ctrl-Q. Live Ctrl-Q is likewise PTY passthrough.
    CtrlQ,
    /// Open the detach confirmation from a reserved live-pane action.
    OpenQuitConfirmation,
    /// workspace scope overlay を開く。
    OpenOverview,
    /// target scope overlay を開く。
    OpenCloseupOverlay,
    /// Open the active target's scratchpad. No keyboard chord is assigned here.
    OpenNotes,
    /// Open the active target's environment editor.
    OpenEnvironment,
    /// Open the active target's Pull Request list overlay.
    OpenPrs,
    /// Open the active target's Markdown preview overlay.
    OpenPreview,
    /// Open the current workspace's durable pending decision list.
    OpenDecisions,
    /// Move within the pending list or current decision options.
    DecisionPrevious,
    /// Move within the pending list or current decision options.
    DecisionNext,
    /// Replace the permitted freeform answer draft.
    SetDecisionFreeform(String),
    /// Submit the selected stable option or nonempty permitted freeform text.
    SubmitDecision,
    /// Choose which scratchpad section the overlay displays.
    SelectNoteSection(NoteSection),
    /// Replace the note editor draft.
    SetNoteDraft(String),
    /// Add the draft as a todo / decision, or apply it as the free-form note.
    CommitNoteDraft,
    /// Toggle a todo without removing checklist entries in bulk.
    ToggleTodo(usize),
    /// Persist the current scratchpad through its owning port.
    SaveNotes,
    /// Insert or replace one environment variable in the local editor.
    SetEnvironment { name: String, value: String },
    /// Persist the current environment through its owning port.
    SaveEnvironment,
    /// 将来の terminal input / command vocabulary 用の文字入力。
    Char(char),
    /// Overview modal の現在の入力を registry 経由で実行する。
    SubmitOverview(String),
    /// Closeup modal の現在の入力を registry 経由で実行する。
    SubmitCloseup(String),
}

/// Converts a non-live terminal input into Home management input.
///
/// This function is deliberately not used for a daemon-owned live pane: callers
/// must route those events through `LiveInputClassifier`, where Ctrl-A and Ctrl-O
/// remain PTY bytes. Some terminals expose Ctrl-A as byte U+0001 while others
/// report a modified `a` or `Home`; all three map to the same Home action here.
#[must_use]
#[coverage(off)]
pub fn classify_management_input(input: LiveInput) -> Option<AppKey> {
    let LiveInput::Key(key) = input else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Char('\u{f}') if !key.modifiers.shift && !key.modifiers.alt => Some(AppKey::CtrlO),
        KeyCode::Char('o')
            if key.modifiers.control && !key.modifiers.shift && !key.modifiers.alt =>
        {
            Some(AppKey::CtrlO)
        }
        KeyCode::Char('\u{e}') if !key.modifiers.shift && !key.modifiers.alt => Some(AppKey::CtrlN),
        KeyCode::Char('n')
            if key.modifiers.control && !key.modifiers.shift && !key.modifiers.alt =>
        {
            Some(AppKey::CtrlN)
        }
        KeyCode::Char('\u{10}') if !key.modifiers.shift && !key.modifiers.alt => {
            Some(AppKey::CtrlP)
        }
        KeyCode::Char('p')
            if key.modifiers.control && !key.modifiers.shift && !key.modifiers.alt =>
        {
            Some(AppKey::CtrlP)
        }
        KeyCode::Char('\u{1}') if !key.modifiers.shift && !key.modifiers.alt => Some(AppKey::CtrlA),
        KeyCode::Char('a')
            if key.modifiers.control && !key.modifiers.shift && !key.modifiers.alt =>
        {
            Some(AppKey::CtrlA)
        }
        KeyCode::Home => Some(AppKey::CtrlA),
        KeyCode::Enter => Some(AppKey::Enter),
        KeyCode::Tab => Some(AppKey::Tab),
        KeyCode::Backspace => Some(AppKey::Backspace),
        KeyCode::Escape => Some(AppKey::Escape),
        KeyCode::Up => Some(AppKey::Up),
        KeyCode::Down => Some(AppKey::Down),
        KeyCode::Left => Some(AppKey::Left),
        KeyCode::Right => Some(AppKey::Right),
        KeyCode::Char(character) if !key.modifiers.control => Some(AppKey::Char(character)),
        _ => None,
    }
}

/// reducer の入力。実 terminal adapter はこの語彙へ変換するだけでよい。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// live terminal input。現行 Home reducer は接続 seam を提供し、pane routing は runtime 合成側が担う。
    Input(LiveInput),
    /// The runtime's current live-pane availability, sampled on every event.
    /// The reducer treats it as a *level* and reacts only on the edge: a
    /// live→non-live transition arms the one-shot Ctrl-C grace and restores the
    /// forced Closeup action modal, while a non-live→live transition drops it. A
    /// re-sampled level that has not changed is inert, so an overlay opened in
    /// the same event batch (quit confirmation, PR / Preview, notes) and the
    /// Ctrl-C grace both survive the next sample.
    LivePaneAvailability(bool),
    /// キー入力。
    Key(AppKey),
    /// terminal size の変更。
    Resize { width: u16, height: u16 },
    /// 定期 tick。
    Tick,
    /// backend snapshot / notice。
    Backend(BackendEvent),
    /// request completion。
    OperationResult(OperationResult),
    /// A pointer gesture over the Home sidebar, in 0-based terminal cells. The
    /// reducer resolves the row with the same viewport geometry the frame draws
    /// and either moves the cursor or, for two presses on the same stable
    /// session identity within 400ms, activates that session. A click outside
    /// the sidebar body clears the pending press and is otherwise inert.
    /// Terminal-pane drag and copy stay a shell +
    /// `TerminalSession` concern and never reach this vocabulary.
    Pointer {
        column: u16,
        row: u16,
        at: std::time::Duration,
    },
}

impl From<RuntimeEvent<BackendEvent>> for AppEvent {
    #[coverage(off)]
    fn from(event: RuntimeEvent<BackendEvent>) -> Self {
        match event {
            RuntimeEvent::Input(input) => Self::Input(input),
            RuntimeEvent::Resize { width, height } => Self::Resize { width, height },
            RuntimeEvent::Tick => Self::Tick,
            RuntimeEvent::Backend(event) => Self::Backend(event),
        }
    }
}

/// backend が TUI-local projection として返す event。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    /// stable identity で表した session snapshot。
    Sessions(Vec<SessionId>),
    /// 表示中 session の name。新規作成の同名 validation にだけ使う advisory copy で、
    /// [`Sessions`](Self::Sessions) の identity 同期とは独立に還流してよい。
    SessionNames(Vec<String>),
    /// backend が safe と保証した notice。
    Notice(Notice),
    /// A phase event for exactly one Agent runtime pane.
    RuntimePhase {
        runtime: AgentRuntimeRef,
        phase: AgentPhase,
    },
    /// Safe progress, error, or connection feedback. Raw protocol details are
    /// deliberately excluded from the TUI event vocabulary.
    Feedback(Feedback),
    /// Scratchpad data returned by its persistence owner.
    NotesLoaded {
        target: Target,
        scratchpad: Scratchpad,
    },
    /// A safe scratchpad read/save failure.
    NotesError { target: Target, error: SafeError },
    /// Environment values returned by the settings owner.
    EnvironmentLoaded {
        target: Target,
        entries: Vec<EnvironmentEntry>,
    },
    /// A safe environment read/save failure.
    EnvironmentError { target: Target, error: SafeError },
    /// Atomic daemon snapshot; records outside `workspace` are rejected by the reducer.
    Decisions {
        workspace: WorkspaceId,
        decisions: Vec<UserDecision>,
    },
    /// Daemon confirmation after resolve.  The item remains visible until this arrives.
    DecisionResolved {
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
    },
    /// A safe resolve failure; the draft and pending item stay retryable.
    DecisionError {
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        error: SafeError,
    },
    /// Pull Request list returned by its snapshot owner for one target.
    PullRequestsLoaded { target: Target, prs: Vec<PrLink> },
    /// A safe Pull Request read failure.
    PullRequestsError { target: Target, error: SafeError },
    /// Markdown preview lines returned by the overlay data owner for one target.
    PreviewLoaded { target: Target, lines: Vec<String> },
    /// A safe preview read failure.
    PreviewError { target: Target, error: SafeError },
}

/// 非同期 request の成否。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationResult {
    /// 完了した request の token。
    pub token: PendingToken,
    /// 成功したか。
    pub succeeded: bool,
    /// Created stable identity, supplied only by a successful daemon lifecycle final.
    pub created: Option<SessionId>,
    /// 画面へ表示してよい補足。失敗時は safe message だけを渡す。
    pub notice: Option<Notice>,
}

/// Pane owner に委譲する tab selection の方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabDirection {
    Next,
    Previous,
}

/// reducer が要求する外部操作。daemon wire 型への変換は adapter 側の責務。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Ask the pane owner to move its stable tab selection without exposing tab
    /// identities to this controller.
    SelectTab { direction: TabDirection },
    /// session create を backend に依頼する。
    CreateSession {
        workspace: WorkspaceId,
        token: PendingToken,
        operation_id: OperationId,
        intent: SessionCreateIntent,
    },
    /// 次の snapshot を要求する。
    RefreshSessions { workspace: WorkspaceId },
    /// workspace scope command を backend adapter に依頼する。
    WorkspaceCommand {
        workspace: WorkspaceId,
        command: overview::Command,
    },
    /// Read an active target's scratchpad through the existing persistence owner.
    LoadNotes { target: Target },
    /// Save an edited scratchpad through the existing persistence owner.
    SaveNotes {
        target: Target,
        scratchpad: Scratchpad,
    },
    /// Read environment values through the existing settings owner.
    LoadEnvironment { target: Target },
    /// Save environment values through the existing settings owner.
    SaveEnvironment {
        target: Target,
        entries: Vec<EnvironmentEntry>,
    },
    /// Fetch the daemon-authoritative pending snapshot for one workspace.
    RefreshDecisions { workspace: WorkspaceId },
    /// Resolve one pending decision using only a locally validated answer.
    ResolveDecision {
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
    },
    /// target の terminal を開くか再利用する。
    OpenTerminal {
        target: Target,
        /// Durable identity used to make a repeated reducer delivery harmless.
        operation_id: OperationId,
        /// Normalized terminal UX mode: `open` or `new`.
        arguments: String,
    },
    /// Start an Agent through the daemon for the active scope. `session` is
    /// absent for a workspace-root Agent. The operation ID is generated by the
    /// TUI and survives acceptance/replay.
    LaunchAgent {
        workspace: WorkspaceId,
        session: Option<SessionId>,
        operation_id: OperationId,
        profile: Option<AgentProfileId>,
    },
    /// selected session を削除する。root はこの effect に変換しない。
    RemoveSession {
        workspace: WorkspaceId,
        session: SessionId,
        force: bool,
    },
    /// Open a workspace and request the Home snapshot for this exact incarnation.
    ///
    /// The identity is deliberately not a name or path: a delayed completion for
    /// a different workspace must never replace the Home currently being opened.
    AttachWorkspace { workspace: WorkspaceId },
    /// Clone a repository through the backend git port, then register the
    /// resulting project through its project/registry ports.
    CloneProject {
        repository: String,
        destination: PathBuf,
        branch: Option<String>,
        token: PendingToken,
    },
    /// Register an already-existing directory through the backend
    /// project/registry ports.
    RegisterWorkspace {
        path: PathBuf,
        name: String,
        token: PendingToken,
    },
    /// Detach this TUI client. The adapter owns the connection cleanup; this
    /// effect intentionally carries no terminal or operation cancellation.
    Detach,
    /// Read a target's Pull Request list through the daemon snapshot owner.
    /// The completion returns as [`BackendEvent::PullRequestsLoaded`] / `Error`.
    LoadPullRequests { target: Target },
    /// Read a target's Markdown preview through the overlay data owner. The
    /// completion returns as [`BackendEvent::PreviewLoaded`] / `Error`.
    LoadPreview { target: Target },
    /// Open one already-selected Pull Request URL through the browser opener.
    /// URL validation stays with the executor; the reducer forwards the raw URL.
    OpenPullRequest { url: String },
}

/// One selectable workspace in the entry surfaces.
///
/// `label` is presentation data only. [`WorkspaceId`] is the identity retained
/// from Welcome / Open through the attach request and into the Home snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryWorkspace {
    /// Stable workspace incarnation.
    pub id: WorkspaceId,
    /// Name rendered by Welcome or Open.
    pub label: String,
}

impl EntryWorkspace {
    /// Create a selectable workspace projection.
    #[must_use]
    #[coverage(off)]
    pub fn new(id: WorkspaceId, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
        }
    }
}

/// The typed part of the first Home response needed to initialize its reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeSnapshot {
    /// The workspace that was attached.
    pub workspace: WorkspaceId,
    /// Session identities in the snapshot order.
    pub sessions: Vec<SessionId>,
}

impl HomeSnapshot {
    /// Create a Home snapshot projection.
    #[must_use]
    #[coverage(off)]
    pub fn new(workspace: WorkspaceId, sessions: Vec<SessionId>) -> Self {
        Self {
            workspace,
            sessions,
        }
    }
}

/// The entry surface currently visible before or after an attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryRoute {
    /// The Welcome menu and its Recent cards.
    Welcome,
    /// The complete registered-workspace list.
    Open,
    /// An attached Home controller.
    Home(Box<AppState>),
}

/// Entry reducer input. The terminal adapter maps concrete keys to this small
/// vocabulary, while tests can drive it without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryEvent {
    /// Move from Welcome to Open.
    ShowOpen,
    /// Select one Single item in Open by its typed identity.
    OpenSingle(WorkspaceId),
    /// Select one Welcome Recent item by its typed identity.
    OpenRecent(WorkspaceId),
    /// Retry the most recent failed attach on the same visible entry surface.
    Retry,
    /// Return from Open to Welcome.
    Back,
    /// Completion for a previously issued attach request.
    AttachResult {
        /// Identity echoed by the backend.
        workspace: WorkspaceId,
        /// A successful typed Home snapshot or a safe in-screen error.
        result: Result<HomeSnapshot, Notice>,
    },
}

/// State for the Welcome → Open / Recent → Home entry flow.
///
/// `opening` is an identity fence. Only a completion for this exact workspace
/// can enter Home; all other (including late) backend results are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryState {
    route: EntryRoute,
    workspaces: Vec<EntryWorkspace>,
    recents: Vec<WorkspaceId>,
    opening: Option<WorkspaceId>,
    failed: Option<WorkspaceId>,
    error: Option<Notice>,
}

impl EntryState {
    /// Create an entry flow at Welcome. Recent IDs may refer to an item absent
    /// from the current Open list; the backend remains authoritative for that
    /// stale registration and reports an in-screen error if it cannot attach.
    #[must_use]
    #[coverage(off)]
    pub fn new(workspaces: Vec<EntryWorkspace>, recents: Vec<WorkspaceId>) -> Self {
        Self {
            route: EntryRoute::Welcome,
            workspaces,
            recents,
            opening: None,
            failed: None,
            error: None,
        }
    }

    /// The current entry route.
    #[must_use]
    #[coverage(off)]
    pub const fn route(&self) -> &EntryRoute {
        &self.route
    }

    /// Registered Open Single choices.
    #[must_use]
    #[coverage(off)]
    pub fn workspaces(&self) -> &[EntryWorkspace] {
        &self.workspaces
    }

    /// Recent typed identities displayed by Welcome.
    #[must_use]
    #[coverage(off)]
    pub fn recents(&self) -> &[WorkspaceId] {
        &self.recents
    }

    /// The attach currently in flight, if any.
    #[must_use]
    #[coverage(off)]
    pub const fn opening(&self) -> Option<WorkspaceId> {
        self.opening
    }

    /// The last attach error, suitable for rendering on the current entry screen.
    #[must_use]
    #[coverage(off)]
    pub fn error(&self) -> Option<&Notice> {
        self.error.as_ref()
    }

    #[coverage(off)]
    fn start_open(&mut self, workspace: WorkspaceId) -> Vec<Effect> {
        if self.opening.is_some() {
            return Vec::new();
        }
        self.opening = Some(workspace);
        self.failed = None;
        self.error = None;
        vec![Effect::AttachWorkspace { workspace }]
    }
}

/// Reduce one entry event and return any backend work it requests.
#[must_use]
#[coverage(off)]
pub fn update_entry(state: &mut EntryState, event: EntryEvent) -> Vec<Effect> {
    match event {
        EntryEvent::ShowOpen if matches!(state.route, EntryRoute::Welcome) => {
            state.route = EntryRoute::Open;
            state.error = None;
            Vec::new()
        }
        EntryEvent::OpenSingle(workspace)
            if matches!(state.route, EntryRoute::Open)
                && state
                    .workspaces
                    .iter()
                    .any(|candidate| candidate.id == workspace) =>
        {
            state.start_open(workspace)
        }
        EntryEvent::OpenRecent(workspace)
            if matches!(state.route, EntryRoute::Welcome) && state.recents.contains(&workspace) =>
        {
            state.start_open(workspace)
        }
        EntryEvent::Retry if state.opening.is_none() => state
            .failed
            .map_or_else(Vec::new, |id| state.start_open(id)),
        EntryEvent::Back if matches!(state.route, EntryRoute::Open) && state.opening.is_none() => {
            state.route = EntryRoute::Welcome;
            state.error = None;
            Vec::new()
        }
        EntryEvent::AttachResult { workspace, result } if state.opening == Some(workspace) => {
            state.opening = None;
            match result {
                Ok(snapshot) if snapshot.workspace == workspace => {
                    state.route = EntryRoute::Home(Box::new(AppState::home(
                        snapshot.workspace,
                        snapshot.sessions,
                    )));
                    state.failed = None;
                    state.error = None;
                }
                Ok(_) => {
                    state.failed = Some(workspace);
                    state.error = Some(Notice::new("workspace changed while opening; retry"));
                }
                Err(error) => {
                    state.failed = Some(workspace);
                    state.error = Some(error);
                }
            }
            Vec::new()
        }
        // A late completion, an invalid selection, an empty list, and keys that
        // do not apply to this screen have no observable state transition.
        _ => Vec::new(),
    }
}

/// Fake entry backend for Welcome / Open attach scenarios. It has no IO: tests
/// inspect dispatched effects and enqueue typed completions in deterministic order.
#[derive(Debug, Default)]
pub struct FakeEntryBackend {
    effects: Vec<Effect>,
    events: VecDeque<EntryEvent>,
}

impl FakeEntryBackend {
    /// Queue one attach completion (including a deliberately stale one).
    #[coverage(off)]
    pub fn push_event(&mut self, event: EntryEvent) {
        self.events.push_back(event);
    }

    /// Effects dispatched by the entry reducer.
    #[must_use]
    #[coverage(off)]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

/// Dispatch entry effects and replay queued fake-backend completions.
#[coverage(off)]
pub fn run_entry_fake_cycle(
    state: &mut EntryState,
    backend: &mut FakeEntryBackend,
    effects: Vec<Effect>,
) {
    backend.effects.extend(effects);
    while let Some(event) = backend.events.pop_front() {
        let _ = update_entry(state, event);
    }
}

/// The New form's two backend-backed operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewMode {
    /// Clone a git repository into a new child directory.
    Clone,
    /// Register an existing directory as a workspace.
    Existing,
}

/// TUI-local editable fields for the New form. The reducer deliberately owns
/// strings rather than presentation widgets, keeping backend validation and
/// retry independent from terminal IO.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewForm {
    pub repository: String,
    pub location: String,
    pub directory: String,
    pub branch: String,
    pub path: String,
    pub name: String,
}

/// A validated request retained across a failed operation so retry never loses
/// the user's form values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewRequest {
    Clone {
        repository: String,
        destination: PathBuf,
        branch: Option<String>,
    },
    Existing {
        path: PathBuf,
        name: String,
    },
}

/// Validation errors that are safe to render in the New form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewValidationError {
    RepositoryRequired,
    LocationRequired,
    DirectoryRequired,
    PathRequired,
    NameRequired,
}

impl NewValidationError {
    #[must_use]
    #[coverage(off)]
    pub const fn message(self) -> &'static str {
        match self {
            Self::RepositoryRequired => "repository URL is required",
            Self::LocationRequired => "clone location is required",
            Self::DirectoryRequired => "directory name is required",
            Self::PathRequired => "directory path is required",
            Self::NameRequired => "workspace name is required",
        }
    }
}

/// Build a backend request from a form after trimming optional whitespace.
///
/// # Errors
///
/// Returns a safe field-specific validation error when a required value is
/// empty after trimming.
#[coverage(off)]
pub fn validate_new_form(mode: NewMode, form: &NewForm) -> Result<NewRequest, NewValidationError> {
    match mode {
        NewMode::Clone => {
            let repository = required(&form.repository, NewValidationError::RepositoryRequired)?;
            let location = required(&form.location, NewValidationError::LocationRequired)?;
            let directory = required(&form.directory, NewValidationError::DirectoryRequired)?;
            let branch = trimmed(&form.branch);
            Ok(NewRequest::Clone {
                repository,
                destination: PathBuf::from(location).join(directory),
                branch,
            })
        }
        NewMode::Existing => Ok(NewRequest::Existing {
            path: PathBuf::from(required(&form.path, NewValidationError::PathRequired)?),
            name: required(&form.name, NewValidationError::NameRequired)?,
        }),
    }
}

#[coverage(off)]
fn required(value: &str, error: NewValidationError) -> Result<String, NewValidationError> {
    trimmed(value).ok_or(error)
}

#[coverage(off)]
fn trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// The New surface either keeps its form or has attached its freshly created
/// workspace to Home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewRoute {
    Form,
    Home(Box<AppState>),
}

/// New-form reducer input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewEvent {
    /// Submit the current form.
    Submit,
    /// Retry the most recently failed backend request without clearing fields.
    Retry,
    /// Backend completion for a pending clone or registration operation.
    Result {
        token: PendingToken,
        result: Result<HomeSnapshot, Notice>,
    },
}

/// Stateful New flow. A token fences late completions, while the form itself is
/// never replaced on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewState {
    route: NewRoute,
    mode: NewMode,
    form: NewForm,
    pending: Option<PendingToken>,
    failed: Option<NewRequest>,
    error: Option<Notice>,
    progress: Option<SafeMessage>,
    next_token: u64,
}

impl NewState {
    #[must_use]
    #[coverage(off)]
    pub fn new(mode: NewMode, form: NewForm) -> Self {
        Self {
            route: NewRoute::Form,
            mode,
            form,
            pending: None,
            failed: None,
            error: None,
            progress: None,
            next_token: 1,
        }
    }

    #[must_use]
    #[coverage(off)]
    pub const fn route(&self) -> &NewRoute {
        &self.route
    }
    #[must_use]
    #[coverage(off)]
    pub const fn mode(&self) -> NewMode {
        self.mode
    }
    #[must_use]
    #[coverage(off)]
    pub const fn form(&self) -> &NewForm {
        &self.form
    }
    #[must_use]
    #[coverage(off)]
    pub const fn pending(&self) -> Option<PendingToken> {
        self.pending
    }
    #[must_use]
    #[coverage(off)]
    pub fn error(&self) -> Option<&Notice> {
        self.error.as_ref()
    }
    #[must_use]
    #[coverage(off)]
    pub fn progress(&self) -> Option<&SafeMessage> {
        self.progress.as_ref()
    }

    #[coverage(off)]
    fn request(&mut self, request: NewRequest) -> Vec<Effect> {
        if self.pending.is_some() {
            return Vec::new();
        }
        let token = PendingToken(self.next_token);
        self.next_token += 1;
        self.pending = Some(token);
        self.failed = None;
        self.error = None;
        match request {
            NewRequest::Clone {
                repository,
                destination,
                branch,
            } => {
                self.progress = Some(SafeMessage::new("Cloning repository…"));
                vec![Effect::CloneProject {
                    repository,
                    destination,
                    branch,
                    token,
                }]
            }
            NewRequest::Existing { path, name } => {
                self.progress = Some(SafeMessage::new("Registering workspace…"));
                vec![Effect::RegisterWorkspace { path, name, token }]
            }
        }
    }
}

/// Reduce one New-form event and return the project/git/registry port request.
#[must_use]
#[coverage(off)]
pub fn update_new(state: &mut NewState, event: NewEvent) -> Vec<Effect> {
    match event {
        NewEvent::Submit if matches!(state.route, NewRoute::Form) && state.pending.is_none() => {
            match validate_new_form(state.mode, &state.form) {
                Ok(request) => state.request(request),
                Err(error) => {
                    state.error = Some(Notice::new(error.message()));
                    Vec::new()
                }
            }
        }
        NewEvent::Retry if matches!(state.route, NewRoute::Form) && state.pending.is_none() => {
            state
                .failed
                .clone()
                .map_or_else(Vec::new, |request| state.request(request))
        }
        NewEvent::Result { token, result } if state.pending == Some(token) => {
            state.pending = None;
            state.progress = None;
            match result {
                Ok(snapshot) => {
                    state.route = NewRoute::Home(Box::new(AppState::home(
                        snapshot.workspace,
                        snapshot.sessions,
                    )));
                    state.failed = None;
                    state.error = None;
                }
                Err(error) => {
                    state.failed = validate_new_form(state.mode, &state.form).ok();
                    state.error = Some(error);
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Backend seam for New. Implementations route clone through git then project
/// registration, and Existing directly through project/registry registration.
pub trait NewProjectPort {
    /// Dispatch one New operation.
    fn dispatch(&mut self, effect: Effect);
    /// Return the next completion, if any.
    fn next_event(&mut self) -> Option<NewEvent>;
}

/// IO-free backend used by New reducer scenarios.
#[derive(Debug, Default)]
pub struct FakeNewBackend {
    effects: Vec<Effect>,
    events: VecDeque<NewEvent>,
}

impl FakeNewBackend {
    #[coverage(off)]
    pub fn push_event(&mut self, event: NewEvent) {
        self.events.push_back(event);
    }
    #[must_use]
    #[coverage(off)]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

impl NewProjectPort for FakeNewBackend {
    #[coverage(off)]
    fn dispatch(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
    #[coverage(off)]
    fn next_event(&mut self) -> Option<NewEvent> {
        self.events.pop_front()
    }
}

/// Dispatch New effects and replay queued fake-backend completions.
#[coverage(off)]
pub fn run_new_fake_cycle(
    state: &mut NewState,
    backend: &mut impl NewProjectPort,
    effects: Vec<Effect>,
) {
    for effect in effects {
        backend.dispatch(effect);
    }
    while let Some(event) = backend.next_event() {
        let _ = update_new(state, event);
    }
}

/// effect を実行し、backend event を取り出す TUI-local port。
pub trait BackendPort {
    /// reducer が返した effect を 1 件 dispatch する。
    fn dispatch(&mut self, effect: Effect);
    /// 次の projection event。無ければ `None`。
    fn next_event(&mut self) -> Option<BackendEvent>;
}

/// reducer scenario 用の backend。request log と event queue のみを持ち、IO はしない。
#[derive(Debug, Default)]
pub struct FakeBackend {
    effects: Vec<Effect>,
    events: VecDeque<BackendEvent>,
}

impl FakeBackend {
    /// backend から届く event を末尾に積む。
    #[coverage(off)]
    pub fn push_event(&mut self, event: BackendEvent) {
        self.events.push_back(event);
    }
    /// dispatch された effect を確認する。
    #[must_use]
    #[coverage(off)]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
    /// effect log を取り出し、空にする。
    #[must_use]
    #[coverage(off)]
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

impl BackendPort for FakeBackend {
    #[coverage(off)]
    fn dispatch(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
    #[coverage(off)]
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.events.pop_front()
    }
}

/// event を state へ還元し、必要な外部 effect を返す。
#[must_use]
#[coverage(off)]
#[allow(clippy::too_many_lines)]
pub fn update(state: &mut AppState, event: AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions,
        }) => {
            if workspace != state.workspace {
                return Vec::new();
            }
            let previously_known = state
                .decisions
                .iter()
                .map(|decision| decision.decision_id)
                .collect::<std::collections::BTreeSet<_>>();
            state.decisions = decisions
                .into_iter()
                .filter(|decision| {
                    decision.owner.workspace_id == workspace
                        && decision.status == UserDecisionStatus::Pending
                })
                .collect();
            state.unread_decisions.retain(|id| {
                state
                    .decisions
                    .iter()
                    .any(|decision| decision.decision_id == *id)
            });
            state.unread_decisions.extend(
                state
                    .decisions
                    .iter()
                    .filter(|decision| !previously_known.contains(&decision.decision_id))
                    .map(|decision| decision.decision_id),
            );
            reconcile_decision_overlay(state);
            // A snapshot is authoritative, but must not repeatedly steal focus
            // after the user dismissed its already-known rows.  A first snapshot
            // (including reconnect/resync) and newly arrived rows open the list
            // only when no other modal/editor owns keyboard input.
            if state.overlay.is_none()
                && state
                    .decisions
                    .iter()
                    .any(|decision| !previously_known.contains(&decision.decision_id))
            {
                state.overlay = Some(Overlay::Decisions);
                state.decision_overlay = Some(DecisionOverlayState {
                    selected: 0,
                    editor: None,
                });
            }
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::DecisionResolved {
            workspace,
            decision_id,
        }) => {
            if workspace != state.workspace {
                return Vec::new();
            }
            state
                .decisions
                .retain(|decision| decision.decision_id != decision_id);
            state.unread_decisions.remove(&decision_id);
            reconcile_decision_overlay(state);
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::DecisionError {
            workspace,
            decision_id,
            error,
        }) => {
            if workspace == state.workspace
                && let Some(editor) = state
                    .decision_overlay
                    .as_mut()
                    .and_then(|overlay| overlay.editor.as_mut())
                    .filter(|editor| editor.decision.decision_id == decision_id)
            {
                editor.error = Some(error);
            }
            Vec::new()
        }
        AppEvent::Backend(event) if update_editor_backend(state, &event) => Vec::new(),
        AppEvent::Key(key) => {
            state.pending_session_click = None;
            update_key(state, key)
        }
        AppEvent::LivePaneAvailability(has_live_pane) => {
            // The runtime samples this level on every event; only an actual edge
            // may move the grace one-shot or the Closeup overlay. A repeated
            // level is inert so an overlay opened in the same batch (quit
            // confirmation, PR / Preview, notes) and the Ctrl-C grace persist.
            if has_live_pane == state.has_live_pane {
                return Vec::new();
            }
            state.ctrl_c_grace = state.has_live_pane && !has_live_pane;
            state.has_live_pane = has_live_pane;
            if matches!(state.route, Route::Home(HomeMode::Closeup)) {
                if has_live_pane {
                    if !state.closeup_action_forced {
                        state.overlay = None;
                    }
                } else {
                    state.overlay = Some(Overlay::Closeup);
                }
            }
            Vec::new()
        }
        AppEvent::Resize { width, height } => {
            state.size = Some((width, height));
            Vec::new()
        }
        AppEvent::Pointer { column, row, at } => update_pointer(state, column, row, at),
        // A live input is classified by `LiveInputClassifier` before reaching
        // this reducer. It still clears a pending grace, because grace is an
        // event-based one-shot rather than a timeout.
        AppEvent::Input(_) => {
            state.ctrl_c_grace = false;
            state.interaction_count = state.interaction_count.saturating_add(1);
            Vec::new()
        }
        AppEvent::Tick => {
            state.mascot_tick = state.mascot_tick.saturating_add(1);
            Vec::new()
        }
        AppEvent::Backend(
            BackendEvent::NotesLoaded { .. }
            | BackendEvent::NotesError { .. }
            | BackendEvent::EnvironmentLoaded { .. }
            | BackendEvent::EnvironmentError { .. }
            | BackendEvent::PullRequestsLoaded { .. }
            | BackendEvent::PullRequestsError { .. }
            | BackendEvent::PreviewLoaded { .. }
            | BackendEvent::PreviewError { .. },
        ) => Vec::new(),
        AppEvent::Backend(BackendEvent::Sessions(sessions)) => {
            // Never combine a press from before an authoritative snapshot with
            // one after it, even when the same stable ID remains visible.
            state.pending_session_click = None;
            state.sessions = sessions;
            state
                .runtimes
                // A workspace-root runtime (no session) is always retained; a
                // session runtime is dropped when its session is gone.
                .retain(|entry| {
                    entry
                        .runtime
                        .session_id
                        .is_none_or(|session| state.sessions.contains(&session))
                });
            state.reconcile_selection();
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::SessionNames(names)) => {
            state.session_names = names;
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Notice(notice)) => {
            state.notice = Some(notice);
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::RuntimePhase { runtime, phase }) => {
            // The runtime's terminal must belong to its own scope, and a session
            // runtime must name a known session. A workspace-root runtime (no
            // session) is always in scope for the active workspace.
            if runtime.terminal.workspace_id != state.workspace
                || runtime.terminal.session_id != runtime.session_id
                || runtime
                    .session_id
                    .is_some_and(|session| !state.sessions.contains(&session))
            {
                return Vec::new();
            }
            let phase = TargetPhase::from_agent_phase(phase);
            if let Some(entry) = state
                .runtimes
                .iter_mut()
                .find(|entry| entry.runtime.fences(&runtime))
            {
                entry.phase = phase;
            } else {
                state.runtimes.push(RuntimePhase { runtime, phase });
            }
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Feedback(feedback)) => {
            state.feedback = Some(feedback);
            Vec::new()
        }
        AppEvent::OperationResult(result) => {
            let pending = state
                .pending
                .iter()
                .position(|pending| pending.token == result.token)
                .map(|index| state.pending.remove(index));
            state.notice = result.notice.clone();
            if result.succeeded {
                if let (Some(pending), Some(created)) = (pending, result.created)
                    && pending.interaction_at_accept == state.interaction_count
                {
                    state.sessions.push(created);
                    state.selected = Selection::Target(Target::Session(created));
                    state.active = Target::Session(created);
                    state.route = Route::Home(HomeMode::Closeup);
                }
            } else if pending.is_some_and(|pending| pending.kind == PendingKind::CreateSession)
                && state.overlay.is_none()
            {
                // A create accepted by the daemon later failed. Surface the safe
                // message as a dismissible dialog over Home. The form was already
                // cleared at submit and the pending row is now removed, so closing
                // the dialog leaves no stale create input or half-created state.
                // A concurrently open overlay keeps the notice fallback instead of
                // being clobbered by the dialog.
                state.create_session_error = result.notice;
                state.overlay = Some(Overlay::CreateSessionError);
            }
            Vec::new()
        }
    }
}

#[coverage(off)]
fn update_editor_backend(state: &mut AppState, event: &BackendEvent) -> bool {
    match event {
        BackendEvent::NotesLoaded { target, scratchpad } => {
            if let Some(editor) = state
                .note_editor
                .as_mut()
                .filter(|editor| editor.target == *target)
            {
                editor.scratchpad.clone_from(scratchpad);
                editor.error = None;
            }
        }
        BackendEvent::NotesError { target, error } => {
            if let Some(editor) = state
                .note_editor
                .as_mut()
                .filter(|editor| editor.target == *target)
            {
                editor.error = Some(error.clone());
            }
        }
        BackendEvent::EnvironmentLoaded { target, entries } => {
            if let Some(editor) = state
                .environment_editor
                .as_mut()
                .filter(|editor| editor.target == *target)
            {
                editor.entries.clone_from(entries);
                editor.error = None;
            }
        }
        BackendEvent::EnvironmentError { target, error } => {
            if let Some(editor) = state
                .environment_editor
                .as_mut()
                .filter(|editor| editor.target == *target)
            {
                editor.error = Some(error.clone());
            }
        }
        BackendEvent::PullRequestsLoaded { target, prs } => {
            if let Some(overlay) = state
                .pr_overlay
                .as_mut()
                .filter(|overlay| overlay.target == *target)
            {
                overlay.prs.clone_from(prs);
                overlay.selected = overlay.selected.min(prs.len().saturating_sub(1));
                overlay.error = None;
            }
        }
        BackendEvent::PullRequestsError { target, error } => {
            if let Some(overlay) = state
                .pr_overlay
                .as_mut()
                .filter(|overlay| overlay.target == *target)
            {
                overlay.error = Some(error.clone());
            }
        }
        BackendEvent::PreviewLoaded { target, lines } => {
            if let Some(overlay) = state
                .preview_overlay
                .as_mut()
                .filter(|overlay| overlay.target == *target)
            {
                overlay.lines.clone_from(lines);
                overlay.error = None;
            }
        }
        BackendEvent::PreviewError { target, error } => {
            if let Some(overlay) = state
                .preview_overlay
                .as_mut()
                .filter(|overlay| overlay.target == *target)
            {
                overlay.error = Some(error.clone());
            }
        }
        _ => return false,
    }
    true
}

#[coverage(off)]
fn update_key(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    state.interaction_count = state.interaction_count.saturating_add(1);
    if let Some(overlay) = state.overlay {
        return update_overlay(state, overlay, key);
    }
    if !matches!(key, AppKey::CtrlC) {
        state.ctrl_c_grace = false;
    }
    match key {
        AppKey::CtrlC => {
            if matches!(state.route, Route::Home(HomeMode::Switch)) {
                return Vec::new();
            }
            if std::mem::take(&mut state.ctrl_c_grace) {
                state.notice = Some(Notice::new("Ctrl-C ignored after leaving live pane"));
                Vec::new()
            } else if state.has_live_pane {
                state.quit_confirm_selected = true;
                state.overlay = Some(Overlay::QuitConfirmation);
                Vec::new()
            } else {
                vec![Effect::Detach]
            }
        }
        AppKey::CtrlQ | AppKey::OpenQuitConfirmation => {
            state.quit_confirm_selected = true;
            state.overlay = Some(Overlay::QuitConfirmation);
            Vec::new()
        }
        AppKey::OpenNotes | AppKey::OpenEnvironment => {
            update_editor_key(state, &key).unwrap_or_default()
        }
        key => update_management_key(state, key),
    }
}

#[coverage(off)]
fn update_overlay(state: &mut AppState, overlay: Overlay, key: AppKey) -> Vec<Effect> {
    // The Closeup action modal exits to Switch on Escape or Ctrl-C: it drops the
    // overlay (and the forced-over-live flag) and returns Home to Switch, the
    // same landing as `Ctrl-O Ctrl-O`. This is symmetric whether the modal is
    // the base surface (no live pane) or forced over a live pane, so the action
    // picker is never a dead-end. Ctrl-Q and every other overlay keep their
    // existing swallow / Escape contracts (below).
    if matches!(overlay, Overlay::Closeup) && matches!(key, AppKey::Escape | AppKey::CtrlC) {
        state.closeup_action_forced = false;
        state.overlay = None;
        state.route = Route::Home(HomeMode::Switch);
        return Vec::new();
    }
    // The create-failure dialog is a single-acknowledge surface: Enter, Escape,
    // or Ctrl-C all dismiss it back to the Home background it was drawn over.
    // Route is untouched, so it never conflicts with the surface the user was on.
    if matches!(overlay, Overlay::CreateSessionError)
        && matches!(key, AppKey::Escape | AppKey::CtrlC | AppKey::Enter)
    {
        state.create_session_error = None;
        state.overlay = None;
        return Vec::new();
    }
    if matches!(key, AppKey::CtrlC | AppKey::CtrlQ) {
        return Vec::new();
    }
    match overlay {
        Overlay::Decisions => update_decisions_overlay(state, key),
        Overlay::QuitConfirmation => match key {
            // `y` forces detach and `n`/Esc forces stay regardless of focus;
            // Enter commits whichever button is focused. Opening the overlay
            // resets focus to Yes, so a bare Ctrl-Q + Enter still detaches.
            AppKey::Char('y' | 'Y') => {
                state.overlay = None;
                vec![Effect::Detach]
            }
            AppKey::Char('n' | 'N') | AppKey::Escape => {
                state.overlay = None;
                Vec::new()
            }
            AppKey::Enter => {
                state.overlay = None;
                if state.quit_confirm_selected {
                    vec![Effect::Detach]
                } else {
                    Vec::new()
                }
            }
            AppKey::Left | AppKey::Right | AppKey::Tab => {
                state.quit_confirm_selected = !state.quit_confirm_selected;
                Vec::new()
            }
            _ => Vec::new(),
        },
        Overlay::Notes | Overlay::Environment => {
            if matches!(key, AppKey::Escape) {
                state.overlay = None;
                state.note_editor = None;
                state.environment_editor = None;
                Vec::new()
            } else {
                update_editor_key(state, &key).unwrap_or_default()
            }
        }
        Overlay::CreateSession => update_create_session_form(state, &key),
        // Dismissal is handled by the early Enter/Escape/Ctrl-C branch above; any
        // other key is inert while the create-failure dialog owns input.
        Overlay::CreateSessionError => Vec::new(),
        Overlay::Prs => update_prs_overlay(state, &key),
        Overlay::Preview => update_preview_overlay(state, &key),
        Overlay::Overview if matches!(key, AppKey::Escape) => {
            state.overlay = None;
            Vec::new()
        }
        Overlay::Overview | Overlay::Closeup => update_management_key(state, key),
    }
}

#[coverage(off)] // Snapshot reconciliation is exercised through update's deterministic decision scenarios.
fn reconcile_decision_overlay(state: &mut AppState) {
    let Some(overlay) = state.decision_overlay.as_mut() else {
        return;
    };
    if let Some(editor) = &overlay.editor
        && !state
            .decisions
            .iter()
            .any(|item| item.decision_id == editor.decision.decision_id)
    {
        overlay.editor = None;
    }
    overlay.selected = overlay
        .selected
        .min(state.decisions.len().saturating_sub(1));
}

#[coverage(off)] // Modal input is covered through update; keeping this helper uninstrumented avoids duplicating reducer accounting.
fn update_decisions_overlay(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    let Some(overlay) = state.decision_overlay.as_mut() else {
        return Vec::new();
    };
    if let Some(editor) = overlay.editor.as_mut() {
        match key {
            AppKey::Escape => {
                overlay.editor = None;
            }
            AppKey::DecisionPrevious | AppKey::Up => {
                editor.selected_option = editor.selected_option.saturating_sub(1);
            }
            AppKey::DecisionNext | AppKey::Down => {
                editor.selected_option = (editor.selected_option + 1)
                    .min(editor.decision.options.len().saturating_sub(1));
            }
            AppKey::SetDecisionFreeform(text) => {
                if editor.decision.allow_freeform {
                    editor.freeform = text;
                    editor.error = None;
                }
            }
            AppKey::Char(ch) if editor.decision.allow_freeform => {
                editor.freeform.push(ch);
                editor.error = None;
            }
            AppKey::Backspace if editor.decision.allow_freeform => {
                editor.freeform.pop();
                editor.error = None;
            }
            AppKey::SubmitDecision | AppKey::Enter => {
                let answer = if editor.decision.allow_freeform && !editor.freeform.trim().is_empty()
                {
                    UserDecisionAnswer::Freeform {
                        text: editor.freeform.trim().to_owned(),
                    }
                } else if let Some(option) = editor.decision.options.get(editor.selected_option) {
                    UserDecisionAnswer::Option {
                        option_id: option.id.clone(),
                    }
                } else {
                    editor.error = Some(SafeError {
                        message: SafeMessage::new("select a valid answer"),
                        error_id: "decision-invalid-answer".to_owned(),
                    });
                    return Vec::new();
                };
                if editor
                    .decision
                    .validate_answer(&answer, chrono::Utc::now())
                    .is_err()
                {
                    editor.error = Some(SafeError {
                        message: SafeMessage::new("select a valid answer"),
                        error_id: "decision-invalid-answer".to_owned(),
                    });
                    return Vec::new();
                }
                return vec![Effect::ResolveDecision {
                    workspace: state.workspace,
                    decision_id: editor.decision.decision_id,
                    answer,
                }];
            }
            _ => {}
        }
    } else {
        match key {
            AppKey::Escape => {
                state.overlay = None;
                state.decision_overlay = None;
            }
            AppKey::DecisionPrevious | AppKey::Up => {
                overlay.selected = overlay.selected.saturating_sub(1);
            }
            AppKey::DecisionNext | AppKey::Down => {
                overlay.selected =
                    (overlay.selected + 1).min(state.decisions.len().saturating_sub(1));
            }
            AppKey::Enter => {
                if let Some(decision) = state.decisions.get(overlay.selected).cloned() {
                    overlay.editor = Some(DecisionEditor::new(decision));
                }
            }
            _ => {}
        }
    }
    Vec::new()
}

#[coverage(off)]
fn update_management_key(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    match key {
        AppKey::OpenDecisions | AppKey::Char('d') => {
            state.unread_decisions.clear();
            state.overlay = Some(Overlay::Decisions);
            state.decision_overlay = Some(DecisionOverlayState {
                selected: 0,
                editor: None,
            });
            vec![Effect::RefreshDecisions {
                workspace: state.workspace,
            }]
        }
        AppKey::Up => {
            state.move_selection(-1);
            Vec::new()
        }
        AppKey::Down => {
            state.move_selection(1);
            Vec::new()
        }
        AppKey::OpenOverview | AppKey::Char(':') => {
            state.overlay = Some(Overlay::Overview);
            Vec::new()
        }
        AppKey::OpenCloseupOverlay => {
            state.overlay = Some(Overlay::Closeup);
            state.closeup_action_forced = state.has_live_pane;
            Vec::new()
        }
        AppKey::CtrlA => match state.route {
            Route::Home(HomeMode::Switch) => open_create_session(state),
            Route::Home(HomeMode::Closeup) => {
                state.overlay = Some(Overlay::Closeup);
                state.closeup_action_forced = state.has_live_pane;
                Vec::new()
            }
        },
        AppKey::CtrlO => {
            if matches!(state.route, Route::Home(HomeMode::Closeup)) {
                state.route = Route::Home(HomeMode::Switch);
                state.closeup_action_forced = false;
                state.overlay = None;
            }
            Vec::new()
        }
        AppKey::CtrlN
            if state.has_live_pane && matches!(state.route, Route::Home(HomeMode::Closeup)) =>
        {
            vec![Effect::SelectTab {
                direction: TabDirection::Next,
            }]
        }
        AppKey::CtrlP
            if state.has_live_pane && matches!(state.route, Route::Home(HomeMode::Closeup)) =>
        {
            vec![Effect::SelectTab {
                direction: TabDirection::Previous,
            }]
        }
        // Switch の `x` / `X` removes only the cursor's session.  Keep this
        // unavailable while an overlay owns input, and never turn the workspace
        // root or the new-session row into a deletion target.
        AppKey::Char('x' | 'X')
            if state.overlay.is_none() && matches!(state.route, Route::Home(HomeMode::Switch)) =>
        {
            remove_selected_session(state, matches!(key, AppKey::Char('X')))
        }
        AppKey::SubmitOverview(input) => submit_overview(state, &input),
        AppKey::SubmitCloseup(input) => submit_closeup(state, &input),
        AppKey::OpenPrs | AppKey::Char('p') => open_prs(state),
        AppKey::OpenPreview | AppKey::Char('v') => open_preview(state),
        AppKey::Enter | AppKey::Char('t') => activate_selected(state),
        AppKey::CtrlN
        | AppKey::CtrlP
        | AppKey::Escape
        | AppKey::Tab
        | AppKey::Left
        | AppKey::Right
        | AppKey::Backspace
        | AppKey::Home
        | AppKey::Char(_)
        | AppKey::CtrlC
        | AppKey::CtrlQ
        | AppKey::OpenQuitConfirmation
        | AppKey::OpenNotes
        | AppKey::OpenEnvironment
        | AppKey::SelectNoteSection(_)
        | AppKey::SetNoteDraft(_)
        | AppKey::CommitNoteDraft
        | AppKey::ToggleTodo(_)
        | AppKey::SaveNotes
        | AppKey::SetEnvironment { .. }
        | AppKey::SaveEnvironment
        | AppKey::DecisionPrevious
        | AppKey::DecisionNext
        | AppKey::SetDecisionFreeform(_)
        | AppKey::SubmitDecision => Vec::new(),
    }
}

/// Request removal for Switch's selected session and leave the cursor on the
/// preceding row while the presentation keeps the target as a loading skeleton.
#[coverage(off)]
fn remove_selected_session(state: &mut AppState, force: bool) -> Vec<Effect> {
    let Selection::Target(Target::Session(session)) = state.selected else {
        return Vec::new();
    };
    state.move_selection(-1);
    vec![Effect::RemoveSession {
        workspace: state.workspace,
        session,
        force,
    }]
}

#[coverage(off)]
fn update_editor_key(state: &mut AppState, key: &AppKey) -> Option<Vec<Effect>> {
    let notes_open = state.overlay == Some(Overlay::Notes);
    let environment_open = state.overlay == Some(Overlay::Environment);
    match key {
        AppKey::OpenNotes => Some(open_notes(state)),
        AppKey::OpenEnvironment => Some(open_environment(state)),
        AppKey::SelectNoteSection(section) => {
            if let Some(editor) = state.note_editor.as_mut().filter(|_| notes_open) {
                editor.section = *section;
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::SetNoteDraft(draft) => {
            if let Some(editor) = state.note_editor.as_mut().filter(|_| notes_open) {
                editor.draft.clone_from(draft);
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::CommitNoteDraft => Some(commit_note_draft(state)),
        AppKey::ToggleTodo(index) => {
            if let Some(editor) = state.note_editor.as_mut().filter(|_| notes_open)
                && let Some(todo) = editor.scratchpad.todos.get_mut(*index)
            {
                todo.done = !todo.done;
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::SaveNotes => Some(
            state
                .note_editor
                .as_ref()
                .filter(|_| notes_open)
                .map_or_else(Vec::new, |editor| {
                    vec![Effect::SaveNotes {
                        target: editor.target,
                        scratchpad: editor.scratchpad.clone(),
                    }]
                }),
        ),
        AppKey::SetEnvironment { name, value } => {
            if let Some(editor) = state
                .environment_editor
                .as_mut()
                .filter(|_| environment_open)
            {
                if let Some(entry) = editor.entries.iter_mut().find(|entry| entry.name == *name) {
                    entry.value.clone_from(value);
                } else if !name.trim().is_empty() {
                    editor.entries.push(EnvironmentEntry {
                        name: name.clone(),
                        value: value.clone(),
                    });
                    editor
                        .entries
                        .sort_by(|left, right| left.name.cmp(&right.name));
                }
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::SaveEnvironment => Some(
            state
                .environment_editor
                .as_ref()
                .filter(|_| environment_open)
                .map_or_else(Vec::new, |editor| {
                    vec![Effect::SaveEnvironment {
                        target: editor.target,
                        entries: editor.entries.clone(),
                    }]
                }),
        ),
        _ => None,
    }
}

#[coverage(off)]
fn open_notes(state: &mut AppState) -> Vec<Effect> {
    let target = state.active;
    state.overlay = Some(Overlay::Notes);
    state.environment_editor = None;
    state.note_editor = Some(NoteEditor::loading(target));
    vec![Effect::LoadNotes { target }]
}

#[coverage(off)]
fn open_environment(state: &mut AppState) -> Vec<Effect> {
    let target = state.active;
    state.overlay = Some(Overlay::Environment);
    state.note_editor = None;
    state.environment_editor = Some(EnvironmentEditor::loading(target));
    vec![Effect::LoadEnvironment { target }]
}

#[coverage(off)]
fn open_prs(state: &mut AppState) -> Vec<Effect> {
    let target = state.active;
    state.overlay = Some(Overlay::Prs);
    state.pr_overlay = Some(PrOverlay::loading(target));
    state.preview_overlay = None;
    vec![Effect::LoadPullRequests { target }]
}

#[coverage(off)]
fn open_preview(state: &mut AppState) -> Vec<Effect> {
    let target = state.active;
    state.overlay = Some(Overlay::Preview);
    state.preview_overlay = Some(PreviewOverlay::loading(target));
    state.pr_overlay = None;
    vec![Effect::LoadPreview { target }]
}

/// Pull Request overlay の入力を還元する。↑↓ で選択を回し、Enter で選択 PR を
/// browser で開く effect を出す。Esc は overlay を閉じる。素材の再取得はしない。
#[coverage(off)]
fn update_prs_overlay(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(overlay) = state.pr_overlay.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.pr_overlay = None;
            Vec::new()
        }
        AppKey::Up => {
            if !overlay.prs.is_empty() {
                overlay.selected = (overlay.selected + overlay.prs.len() - 1) % overlay.prs.len();
            }
            Vec::new()
        }
        AppKey::Down => {
            if !overlay.prs.is_empty() {
                overlay.selected = (overlay.selected + 1) % overlay.prs.len();
            }
            Vec::new()
        }
        AppKey::Enter => overlay
            .selected_pr()
            .map(|pr| Effect::OpenPullRequest {
                url: pr.url.clone(),
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Preview overlay の入力を還元する。↑↓ で scroll し、Esc は overlay を閉じる。
#[coverage(off)]
fn update_preview_overlay(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(overlay) = state.preview_overlay.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.preview_overlay = None;
        }
        AppKey::Up => overlay.scroll = overlay.scroll.saturating_sub(1),
        AppKey::Down => overlay.scroll = overlay.scroll.saturating_add(1),
        _ => {}
    }
    Vec::new()
}

#[coverage(off)]
fn commit_note_draft(state: &mut AppState) -> Vec<Effect> {
    let Some(editor) = state
        .note_editor
        .as_mut()
        .filter(|_| state.overlay == Some(Overlay::Notes))
    else {
        return Vec::new();
    };
    let draft = editor.draft.trim();
    match editor.section {
        NoteSection::Note => editor.scratchpad.note = (!draft.is_empty()).then(|| draft.to_owned()),
        NoteSection::Todos if !draft.is_empty() => editor
            .scratchpad
            .todos
            .push(usagi_core::domain::note::SessionTodo::new(draft)),
        NoteSection::Decisions if !draft.is_empty() => {
            editor
                .scratchpad
                .decisions
                .push(usagi_core::domain::note::SessionDecision::new(
                    chrono::Utc::now(),
                    draft,
                ));
        }
        NoteSection::Todos | NoteSection::Decisions => {}
    }
    editor.draft.clear();
    editor.error = None;
    Vec::new()
}

#[coverage(off)]
fn submit_overview(state: &mut AppState, input: &str) -> Vec<Effect> {
    if state.overlay != Some(Overlay::Overview) {
        return Vec::new();
    }
    match overview::interpret(input) {
        Ok(overview::Command::Env { .. }) => open_environment(state),
        Ok(overview::Command::Session { arguments }) => submit_overview_session(state, &arguments),
        Ok(command) => {
            state.overlay = None;
            state.notice = Some(Notice::new(format!("Requested {}", command.name())));
            vec![Effect::WorkspaceCommand {
                workspace: state.workspace,
                command,
            }]
        }
        Err(error) => {
            state.notice = Some(Notice::new(error.to_string()));
            Vec::new()
        }
    }
}

#[coverage(off)]
fn submit_overview_session(state: &mut AppState, arguments: &str) -> Vec<Effect> {
    let command = match overview::parse_session(arguments) {
        Ok(command) => command,
        Err(message) => {
            state.notice = Some(Notice::new(message));
            return Vec::new();
        }
    };
    match command {
        overview::SessionCommand::Create { name } => {
            state.overlay = None;
            request_create_session(
                state,
                SessionCreateIntent {
                    name,
                    profile: None,
                    model: None,
                },
            )
        }
        overview::SessionCommand::List | overview::SessionCommand::Overview => {
            state.overlay = None;
            state.notice = Some(Notice::new("Refreshing sessions"));
            vec![Effect::RefreshSessions {
                workspace: state.workspace,
            }]
        }
        overview::SessionCommand::SelectRemove { .. } => {
            state.notice = Some(Notice::new(
                "session selection is available in the live TUI",
            ));
            Vec::new()
        }
        overview::SessionCommand::Remove { .. } => {
            state.notice = Some(Notice::new(
                "named session removal is available in the live TUI",
            ));
            Vec::new()
        }
    }
}

#[coverage(off)]
fn submit_closeup(state: &mut AppState, input: &str) -> Vec<Effect> {
    if state.overlay != Some(Overlay::Closeup) {
        return Vec::new();
    }
    let command = match closeup::interpret(input) {
        Ok(command) => command,
        Err(error) => {
            state.notice = Some(Notice::new(error.to_string()));
            return Vec::new();
        }
    };
    let command_name = command.name();
    let effect = match command {
        closeup::Command::Terminal { arguments } => match terminal_arguments(&arguments) {
            Ok(arguments) => Some(Effect::OpenTerminal {
                target: state.active,
                operation_id: OperationId::new(),
                arguments,
            }),
            Err(error) => {
                state.notice = Some(error);
                None
            }
        },
        closeup::Command::Agent { arguments } => match optional_profile(&arguments) {
            // A workspace-root Agent (`Target::Root`) runs in the trusted
            // repository root; a session Agent runs in that session's worktree.
            // The daemon resolves the checkout path in both cases.
            Ok(profile) => Some(Effect::LaunchAgent {
                workspace: state.workspace,
                session: state.active.session_id(),
                operation_id: OperationId::new(),
                profile,
            }),
            Err(error) => {
                state.notice = Some(error);
                None
            }
        },
        closeup::Command::Close { arguments } => match state.active {
            Target::Session(session) => {
                if let Some(force) = parse_close_force(&arguments) {
                    Some(Effect::RemoveSession {
                        workspace: state.workspace,
                        session,
                        force,
                    })
                } else {
                    state.notice = Some(Notice::new("invalid close arguments"));
                    None
                }
            }
            Target::Root(_) => {
                state.notice = Some(Notice::new("workspace root cannot be closed"));
                None
            }
        },
        closeup::Command::Diff { .. } => {
            state.notice = Some(Notice::new(format!("{command_name} is not available")));
            None
        }
    };
    if effect.is_some() {
        state.overlay = None;
        state.notice = Some(Notice::new(format!("Requested {command_name}")));
    }
    effect.into_iter().collect()
}

/// Normalize the two supported terminal forms at the controller boundary.
/// Empty input is intentionally `open`: it reuses an exact daemon-owned
/// terminal when one exists and launches only when inventory is empty.
fn terminal_arguments(arguments: &str) -> Result<String, Notice> {
    match arguments.trim() {
        "" | "open" => Ok("open".to_owned()),
        "new" => Ok("new".to_owned()),
        _ => Err(Notice::new("terminal accepts only `open` or `new`")),
    }
}

#[coverage(off)]
fn parse_close_force(arguments: &str) -> Option<bool> {
    crate::usecase::session_remove::parse(arguments)
        .ok()
        .filter(|request| request.target.is_none())
        .map(|request| request.force)
}

/// Maximum elapsed time between presses on one stable session identity.
const SIDEBAR_DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// Resolve a Home sidebar press and apply it. An open overlay (a modal or the
/// inline create form) owns the pointer, so a background click is inert and
/// invalidates any earlier first press. Root, `+ new session`, and misses retain
/// their single-click selection behavior but cannot become double clicks.
fn update_pointer(
    state: &mut AppState,
    column: u16,
    row: u16,
    at: std::time::Duration,
) -> Vec<Effect> {
    if state.overlay.is_some() {
        state.pending_session_click = None;
        return Vec::new();
    }
    let Some(selection) = state.sidebar_selection_at(column, row) else {
        state.pending_session_click = None;
        return Vec::new();
    };
    state.select_row(selection);
    let Selection::Target(Target::Session(session)) = selection else {
        state.pending_session_click = None;
        return Vec::new();
    };
    let doubled = state
        .pending_session_click
        .is_some_and(|(previous, previous_at)| {
            previous == session
                && at
                    .checked_sub(previous_at)
                    .is_some_and(|elapsed| elapsed <= SIDEBAR_DOUBLE_CLICK)
        });
    if doubled {
        // Consume both presses so a third press starts a fresh pair.
        state.pending_session_click = None;
        activate_selected(state)
    } else {
        state.pending_session_click = Some((session, at));
        Vec::new()
    }
}

#[coverage(off)]
fn activate_selected(state: &mut AppState) -> Vec<Effect> {
    match state.selected {
        Selection::Target(target) => {
            state.active = target;
            state.route = Route::Home(HomeMode::Closeup);
            state.closeup_action_forced = false;
            state.overlay = (!state.has_live_pane).then_some(Overlay::Closeup);
            Vec::new()
        }
        Selection::NewSession => open_create_session(state),
    }
}

#[coverage(off)]
fn open_create_session(state: &mut AppState) -> Vec<Effect> {
    // Ctrl-A opens this persistent sidebar action directly, so keep the visual
    // cursor and the inline form on the same `+ new session` row. The active
    // target remains unchanged.
    state.selected = Selection::NewSession;
    state.create_session = Some(CreateSessionForm::new(state.session_names.clone()));
    state.overlay = Some(Overlay::CreateSession);
    Vec::new()
}

#[coverage(off)]
fn update_create_session_form(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(form) = state.create_session.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    match key {
        AppKey::Escape => {
            state.create_session = None;
            state.overlay = None;
            Vec::new()
        }
        AppKey::Backspace => {
            form.backspace();
            Vec::new()
        }
        AppKey::Char(character) if !character.is_control() => {
            form.push(*character);
            Vec::new()
        }
        AppKey::Enter => match form.request() {
            Ok(intent) => {
                state.create_session = None;
                state.overlay = None;
                request_create_session(state, intent)
            }
            Err(error) => {
                form.error = Some(error);
                Vec::new()
            }
        },
        // Ctrl-A/Home/Tab and unsupported keys must never retrigger create or edit
        // a removed field while this name-only form owns input.
        _ => Vec::new(),
    }
}

#[coverage(off)]
fn request_create_session(state: &mut AppState, intent: SessionCreateIntent) -> Vec<Effect> {
    let token = PendingToken(state.next_pending_token);
    state.next_pending_token += 1;
    let operation_id = OperationId::new();
    state.pending.push(PendingOperation {
        token,
        kind: PendingKind::CreateSession,
        operation_id,
        interaction_at_accept: state.interaction_count,
    });
    vec![Effect::CreateSession {
        workspace: state.workspace,
        token,
        operation_id,
        intent,
    }]
}

/// effect を dispatch し、queue 済みの backend event を reducer へ戻すテスト helper。
#[coverage(off)]
pub fn run_fake_cycle(state: &mut AppState, backend: &mut impl BackendPort, effects: Vec<Effect>) {
    for effect in effects {
        backend.dispatch(effect);
    }
    while let Some(event) = backend.next_event() {
        let _ = update(state, AppEvent::Backend(event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_arguments_normalize_open_and_reject_untrusted_input() {
        assert_eq!(terminal_arguments("").unwrap(), "open");
        assert_eq!(terminal_arguments(" open ").unwrap(), "open");
        assert_eq!(terminal_arguments("new").unwrap(), "new");
        assert_eq!(
            terminal_arguments("--command sh").unwrap_err().message,
            "terminal accepts only `open` or `new`"
        );
    }
    use usagi_core::domain::id::{
        AgentRuntimeId, DaemonGeneration, TerminalId, TerminalRef, WorktreeId,
    };

    fn ids() -> (WorkspaceId, SessionId, SessionId) {
        (WorkspaceId::new(), SessionId::new(), SessionId::new())
    }

    fn sized_home(
        workspace: WorkspaceId,
        sessions: Vec<SessionId>,
        width: u16,
        height: u16,
    ) -> AppState {
        let mut state = AppState::home(workspace, sessions);
        let _ = update(&mut state, AppEvent::Resize { width, height });
        state
    }

    fn click_at(state: &mut AppState, column: u16, row: u16, at_ms: u64) -> Selection {
        let _ = update(
            state,
            AppEvent::Pointer {
                column,
                row,
                at: std::time::Duration::from_millis(at_ms),
            },
        );
        state.selected()
    }

    #[test]
    fn pointer_click_resolves_and_selects_each_sidebar_row() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = sized_home(workspace, vec![session], 100, 30);

        // Content begins after the two chrome rows: the Root identity line (row 2),
        // the divider (row 3), the session's two lines (rows 4-5), then the action
        // row (row 6). Each click moves the navigation cursor to that row.
        assert_eq!(
            click_at(&mut state, 5, 2, 0),
            Selection::Target(Target::Root(workspace))
        );
        assert_eq!(
            click_at(&mut state, 5, 4, 1_000),
            Selection::Target(Target::Session(session))
        );
        assert_eq!(
            click_at(&mut state, 5, 5, 2_000),
            Selection::Target(Target::Session(session))
        );
        assert_eq!(click_at(&mut state, 5, 6, 3_000), Selection::NewSession);

        // The divider under Root and a click below every rendered row select
        // nothing new: the cursor stays where it last landed.
        let before = state.selected();
        let effects = update(
            &mut state,
            AppEvent::Pointer {
                column: 5,
                row: 3,
                at: std::time::Duration::from_millis(4_000),
            },
        );
        assert!(effects.is_empty());
        assert_eq!(state.selected(), before);
        let _ = click_at(&mut state, 5, 8, 5_000);
        assert_eq!(state.selected(), before);
    }

    #[test]
    fn pointer_click_outside_the_sidebar_body_is_inert() {
        let workspace = WorkspaceId::new();
        // A pointer before the first resize has no geometry and cannot resolve.
        let mut ungeometried = AppState::home(workspace, Vec::new());
        assert!(
            update(
                &mut ungeometried,
                AppEvent::Pointer {
                    column: 5,
                    row: 2,
                    at: std::time::Duration::ZERO,
                },
            )
            .is_empty()
        );
        assert_eq!(
            ungeometried.selected(),
            Selection::Target(Target::Root(workspace))
        );

        let mut state = sized_home(workspace, Vec::new(), 100, 30);
        let root = Selection::Target(Target::Root(workspace));
        for (column, row) in [
            (90, 4), // right-pane column
            (5, 0),  // header row
            (5, 1),  // spacer row
        ] {
            let _ = click_at(&mut state, column, row, u64::from(row));
            assert_eq!(state.selected(), root);
        }
        // Zero dimensions fall back to 80x24, so a mid-sidebar click still lands.
        let mut zeroed = sized_home(workspace, Vec::new(), 0, 0);
        assert_eq!(click_at(&mut zeroed, 5, 2, 0), root);
        // A viewport at or under the chrome, and a click past the content
        // capacity, both resolve to nothing.
        let tiny = sized_home(workspace, Vec::new(), 100, 2);
        assert!(tiny.sidebar_selection_at(5, 2).is_none());
        let short = sized_home(workspace, vec![SessionId::new()], 100, 8);
        assert!(short.sidebar_selection_at(5, 7).is_none());
    }

    #[test]
    fn pointer_click_handles_single_body_line_and_overflow() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        // body_height == 1 (height 3): only the first row is addressable.
        let single = sized_home(workspace, vec![session], 100, 3);
        assert_eq!(
            single.sidebar_selection_at(5, 2),
            Some(Selection::Target(Target::Root(workspace)))
        );
        assert_eq!(single.sidebar_selection_at(5, 5), None);
        // At height 6 the content capacity is 3, so the session's two lines
        // overflow after the Root row and divider and cannot be hit.
        let overflow = sized_home(workspace, vec![session], 100, 6);
        assert_eq!(overflow.sidebar_selection_at(5, 4), None);
    }

    #[test]
    fn pointer_click_reaches_the_scrolled_viewport_tail() {
        let workspace = WorkspaceId::new();
        let sessions: Vec<SessionId> = (0..6).map(|_| SessionId::new()).collect();
        // A short viewport cannot show every row at once. Moving the cursor to the
        // tail (`+ new session`) scrolls the list, and a click on the last body row
        // still resolves to the row the frame now shows there.
        let mut state = sized_home(workspace, sessions.clone(), 100, 10);
        for _ in 0..=sessions.len() {
            let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        }
        assert_eq!(state.selected(), Selection::NewSession);
        // The short viewport scrolled to the tail: the last visible session and the
        // action row. The mascot reserves the sidebar foot, so only the top three
        // body rows are clickable and the action sits on the last of them (row 4).
        let hit = state
            .sidebar_selection_at(5, 4)
            .expect("the tail row is addressable once scrolled");
        assert_eq!(hit, Selection::NewSession);
        let _ = click_at(&mut state, 5, 4, 0);
        assert_eq!(state.selected(), Selection::NewSession);
    }

    #[test]
    fn session_pointer_single_click_selects_and_double_click_matches_enter() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = sized_home(workspace, vec![session], 100, 30);

        assert_eq!(
            click_at(&mut state, 5, 4, 1_000),
            Selection::Target(Target::Session(session))
        );
        assert_eq!(state.active(), Target::Root(workspace));
        assert!(matches!(state.route(), Route::Home(HomeMode::Switch)));
        assert_eq!(state.overlay(), None);

        // The inclusive 400ms boundary is the same activation path as Enter.
        assert_eq!(
            click_at(&mut state, 5, 4, 1_400),
            Selection::Target(Target::Session(session))
        );
        assert_eq!(state.active(), Target::Session(session));
        assert!(matches!(state.route(), Route::Home(HomeMode::Closeup)));
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
    }

    #[test]
    fn session_pointer_outside_window_starts_a_new_pair_and_regressed_time_is_safe() {
        let (workspace, session, _) = ids();
        let mut state = sized_home(workspace, vec![session], 100, 30);
        let _ = click_at(&mut state, 5, 4, 1_000);
        let _ = click_at(&mut state, 5, 4, 1_401);
        assert_eq!(state.active(), Target::Root(workspace));
        let _ = click_at(&mut state, 5, 4, 1_200);
        assert_eq!(state.active(), Target::Root(workspace));
        let _ = click_at(&mut state, 5, 4, 1_600);
        assert_eq!(state.active(), Target::Session(session));
    }

    #[test]
    fn non_session_pointer_hits_invalidate_the_pending_session_press() {
        let (workspace, session, _) = ids();
        for (column, row) in [(5, 2), (5, 6), (5, 3), (90, 4)] {
            let mut state = sized_home(workspace, vec![session], 100, 30);
            let _ = click_at(&mut state, 5, 4, 1_000);
            let _ = click_at(&mut state, column, row, 1_100);
            let _ = click_at(&mut state, 5, 4, 1_200);
            assert_eq!(state.active(), Target::Root(workspace));
        }
    }

    #[test]
    fn another_session_and_scrolled_same_cell_do_not_activate() {
        let workspace = WorkspaceId::new();
        let sessions: Vec<SessionId> = (0..6).map(|_| SessionId::new()).collect();
        let mut other = sized_home(workspace, sessions[..2].to_vec(), 100, 30);
        let _ = click_at(&mut other, 5, 4, 1_000);
        let _ = click_at(&mut other, 5, 6, 1_100);
        assert_eq!(other.active(), Target::Root(workspace));

        let mut scrolled = sized_home(workspace, sessions.clone(), 100, 14);
        let mut tail = scrolled.clone();
        tail.selected = Selection::NewSession;
        let (row, first, second) = (2_u16..14)
            .find_map(|row| {
                match (
                    scrolled.sidebar_selection_at(5, row),
                    tail.sidebar_selection_at(5, row),
                ) {
                    (
                        Some(Selection::Target(Target::Session(first))),
                        Some(Selection::Target(Target::Session(second))),
                    ) if first != second => Some((row, first, second)),
                    _ => None,
                }
            })
            .expect("scrolling replaces a visible session cell with another identity");
        let _ = click_at(&mut scrolled, 5, row, 1_000);
        scrolled.selected = Selection::NewSession;
        assert_ne!(first, second);
        let _ = click_at(&mut scrolled, 5, row, 1_100);
        assert_eq!(scrolled.active(), Target::Root(workspace));
    }

    #[test]
    fn session_snapshot_invalidates_pending_press_even_when_identity_remains() {
        let (workspace, session, _) = ids();
        let mut state = sized_home(workspace, vec![session], 100, 30);
        let _ = click_at(&mut state, 5, 4, 1_000);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(vec![session])),
        );
        let _ = click_at(&mut state, 5, 4, 1_100);
        assert_eq!(state.active(), Target::Root(workspace));
        let _ = click_at(&mut state, 5, 4, 1_500);
        assert_eq!(state.active(), Target::Session(session));
    }

    #[test]
    fn consumed_double_click_does_not_turn_a_third_press_into_activation() {
        let (workspace, session, _) = ids();
        let mut state = sized_home(workspace, vec![session], 100, 30);
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = click_at(&mut state, 5, 4, 1_000);
        let _ = click_at(&mut state, 5, 4, 1_100);
        assert_eq!(state.active(), Target::Session(session));
        state.active = Target::Root(workspace);
        state.route = Route::Home(HomeMode::Switch);
        let _ = click_at(&mut state, 5, 4, 1_200);
        assert_eq!(state.active(), Target::Root(workspace));
    }

    #[test]
    fn pointer_click_is_inert_while_an_overlay_owns_the_surface() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = sized_home(workspace, vec![session], 100, 30);
        // Open the workspace Overview overlay, then click a background session row.
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        assert_eq!(state.overlay(), Some(Overlay::Overview));
        let before = state.selected();
        state.pending_session_click = Some((session, std::time::Duration::from_millis(1_000)));
        let effects = update(
            &mut state,
            AppEvent::Pointer {
                column: 5,
                row: 4,
                at: std::time::Duration::from_millis(1_100),
            },
        );
        assert!(effects.is_empty());
        assert_eq!(state.selected(), before);
        assert_eq!(state.overlay(), Some(Overlay::Overview));
        assert!(state.pending_session_click.is_none());
        state.overlay = None;
        let _ = click_at(&mut state, 5, 4, 1_200);
        assert_eq!(state.active(), Target::Root(workspace));

        // The inline create form owns the same background pointer boundary.
        state.overlay = Some(Overlay::CreateSession);
        let _ = update(
            &mut state,
            AppEvent::Pointer {
                column: 5,
                row: 4,
                at: std::time::Duration::from_millis(1_300),
            },
        );
        assert!(state.pending_session_click.is_none());
        state.overlay = None;
        let _ = click_at(&mut state, 5, 4, 1_400);
        assert_eq!(state.active(), Target::Root(workspace));
    }

    #[test]
    fn create_session_form_edits_the_name_only_and_defaults_profile_and_model() {
        let mut form = CreateSessionForm::default();
        assert_eq!(form.name(), "");
        assert!(form.error().is_none());

        assert!(required_create_value(" ", "required").is_err());
        assert_eq!(required_create_value(" name ", "required").unwrap(), "name");

        for character in "sessio".chars() {
            form.push(character);
        }
        form.backspace();
        form.push('o');
        form.push('n');

        let request = form.request().unwrap();
        assert_eq!(request.name, "session");
        // profile / model are no longer part of the create flow: the intent always
        // defers to the daemon's workspace default policy.
        assert!(request.profile.is_none());
        assert!(request.model.is_none());
    }

    #[test]
    fn optional_profile_maps_blank_valid_and_invalid_inputs() {
        // `optional_profile` still backs the Closeup `agent [profile]` command, so
        // its blank / valid / invalid branches (including the error closure) stay
        // exercised even though the create form no longer takes a profile.
        assert_eq!(optional_profile("").unwrap(), None);
        assert_eq!(
            optional_profile(" codex ").unwrap().unwrap().as_str(),
            "codex"
        );
        assert!(optional_profile("invalid profile").is_err());
    }

    #[test]
    fn create_session_form_defers_the_empty_name_error_to_submit() {
        // While typing nothing, the empty name is "in progress", not an error.
        let mut form = CreateSessionForm::new(Vec::new());
        form.push(' ');
        assert!(
            form.error().is_none(),
            "whitespace-only is not a live error"
        );
        // Submitting an effectively empty name surfaces the required-name error and
        // keeps the draft so the user can keep typing.
        let error = form.request().unwrap_err();
        assert_eq!(error.message, "session name is required");
        assert_eq!(form.name(), " ", "draft is preserved after a failed submit");
    }

    #[test]
    fn create_session_form_rejects_invalid_characters_while_typing() {
        let mut form = CreateSessionForm::new(Vec::new());
        for character in "ok".chars() {
            form.push(character);
        }
        assert!(form.error().is_none());
        form.push('/');
        assert_eq!(form.error().unwrap().message, "invalid character");
        // Submitting keeps the draft and refuses to build a request.
        assert!(form.request().is_err());
        assert_eq!(form.name(), "ok/");
        // Fixing the input clears the error and lets the request through.
        form.backspace();
        assert!(form.error().is_none());
        assert_eq!(form.request().unwrap().name, "ok");
    }

    #[test]
    fn create_session_form_rejects_names_longer_than_the_limit() {
        let mut form = CreateSessionForm::new(Vec::new());
        for _ in 0..=MAX_SESSION_NAME_LEN {
            form.push('a');
        }
        assert_eq!(form.error().unwrap().message, "name too long (max 64)");
        assert!(form.request().is_err());
        // A name exactly at the limit is accepted.
        form.backspace();
        assert!(form.error().is_none());
        assert_eq!(form.name().chars().count(), MAX_SESSION_NAME_LEN);
        assert_eq!(
            form.request().unwrap().name.chars().count(),
            MAX_SESSION_NAME_LEN
        );
    }

    #[test]
    fn create_session_form_rejects_a_duplicate_of_a_displayed_session() {
        let mut form = CreateSessionForm::new(vec!["alpha".to_owned()]);
        for character in "alpha".chars() {
            form.push(character);
        }
        assert_eq!(form.error().unwrap().message, "name already exists");
        assert!(form.request().is_err());
        assert_eq!(form.name(), "alpha", "draft is preserved");
        // A distinct name is accepted; the duplicate check is against the exact name.
        form.push('-');
        form.push('2');
        assert!(form.error().is_none());
        assert_eq!(form.request().unwrap().name, "alpha-2");
    }

    #[test]
    fn open_create_session_seeds_the_form_with_displayed_names() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::SessionNames(vec!["alpha".to_owned()])),
        );
        // Down reaches `+ new session`; Enter opens the form seeded with the names.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        for character in "alpha".chars() {
            let _ = update(&mut state, AppEvent::Key(AppKey::Char(character)));
        }
        let form = state.create_session_form().unwrap();
        assert_eq!(form.error().unwrap().message, "name already exists");
    }

    #[test]
    fn management_classifier_preserves_closeup_control_chords() {
        let ctrl_a = |code| {
            LiveInput::Key(crate::usecase::terminal_input::KeyEvent::new(
                code,
                crate::usecase::terminal_input::Modifiers {
                    control: true,
                    ..crate::usecase::terminal_input::Modifiers::default()
                },
                KeyEventKind::Press,
            ))
        };
        assert_eq!(
            classify_management_input(ctrl_a(KeyCode::Char('\u{1}'))),
            Some(AppKey::CtrlA)
        );
        assert_eq!(
            classify_management_input(ctrl_a(KeyCode::Char('a'))),
            Some(AppKey::CtrlA)
        );
        assert_eq!(
            classify_management_input(LiveInput::Key(
                crate::usecase::terminal_input::KeyEvent::new(
                    KeyCode::Home,
                    crate::usecase::terminal_input::Modifiers::default(),
                    KeyEventKind::Press,
                )
            )),
            Some(AppKey::CtrlA)
        );
        assert_eq!(
            classify_management_input(LiveInput::Key(
                crate::usecase::terminal_input::KeyEvent::new(
                    KeyCode::Char('\u{f}'),
                    crate::usecase::terminal_input::Modifiers::default(),
                    KeyEventKind::Press,
                ),
            )),
            Some(AppKey::CtrlO)
        );
        for code in [KeyCode::Char('\u{f}'), KeyCode::Char('o')] {
            assert_eq!(classify_management_input(ctrl_a(code)), Some(AppKey::CtrlO));
        }
        for code in [KeyCode::Char('\u{e}'), KeyCode::Char('n')] {
            assert_eq!(classify_management_input(ctrl_a(code)), Some(AppKey::CtrlN));
        }
        for code in [KeyCode::Char('\u{10}'), KeyCode::Char('p')] {
            assert_eq!(classify_management_input(ctrl_a(code)), Some(AppKey::CtrlP));
        }
    }

    #[test]
    fn management_classifier_keeps_navigation_ctrl_a_and_ignores_caret_only_keys() {
        use crate::usecase::terminal_input::Modifiers;
        let key = |code, modifiers| {
            LiveInput::Key(crate::usecase::terminal_input::KeyEvent::new(
                code,
                modifiers,
                KeyEventKind::Press,
            ))
        };
        let plain = Modifiers::default;
        let shift = || Modifiers {
            shift: true,
            ..plain()
        };
        // Home and Ctrl-A remain the `+ new session` action in navigation; this
        // reducer has no byte caret, so the caret-only edits (End / Ctrl-E,
        // Delete, and Shift+motion selection) are inert here and never
        // mis-route into the string-only create form.
        assert_eq!(
            classify_management_input(key(KeyCode::Home, plain())),
            Some(AppKey::CtrlA)
        );
        assert_eq!(classify_management_input(key(KeyCode::End, plain())), None);
        assert_eq!(
            classify_management_input(key(
                KeyCode::Char('e'),
                Modifiers {
                    control: true,
                    ..plain()
                }
            )),
            None
        );
        assert_eq!(
            classify_management_input(key(KeyCode::Delete, plain())),
            None
        );
        assert_eq!(
            classify_management_input(key(KeyCode::Home, shift())),
            Some(AppKey::CtrlA),
            "Shift+Home still resolves to the navigation action, not a selection"
        );
    }

    fn clone_form() -> NewForm {
        NewForm {
            repository: " https://example.com/acme/app.git ".to_owned(),
            location: " /work ".to_owned(),
            directory: " app ".to_owned(),
            branch: " main ".to_owned(),
            ..NewForm::default()
        }
    }

    fn existing_form() -> NewForm {
        NewForm {
            path: " /work/existing ".to_owned(),
            name: " existing ".to_owned(),
            ..NewForm::default()
        }
    }

    fn runtime(workspace: WorkspaceId, session: SessionId) -> AgentRuntimeRef {
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
        .unwrap()
    }

    #[test]
    fn home_starts_with_root_selected_and_active() {
        let (workspace, first, second) = ids();
        let state = AppState::home(workspace, vec![first, second]);
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        assert_eq!(state.selected(), Selection::Target(Target::Root(workspace)));
        assert_eq!(state.active(), Target::Root(workspace));
        assert_eq!(state.sessions(), &[first, second]);
    }

    #[test]
    fn tick_advances_only_the_mascot_animation_frame() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(state.mascot_tick(), 0);
        let _ = update(&mut state, AppEvent::Tick);
        assert_eq!(state.mascot_tick(), 1);
    }

    #[test]
    #[coverage(off)]
    fn new_clone_validates_dispatches_progress_and_attaches_home_on_success() {
        let (workspace, session, _) = ids();
        let mut state = NewState::new(NewMode::Clone, clone_form());
        let mut backend = FakeNewBackend::default();
        let effects = update_new(&mut state, NewEvent::Submit);
        assert_eq!(state.pending(), Some(PendingToken(1)));
        assert_eq!(
            state.progress().map(SafeMessage::as_str),
            Some("Cloning repository…")
        );
        assert_eq!(
            effects,
            vec![Effect::CloneProject {
                repository: "https://example.com/acme/app.git".to_owned(),
                destination: PathBuf::from("/work/app"),
                branch: Some("main".to_owned()),
                token: PendingToken(1),
            }]
        );
        backend.push_event(NewEvent::Result {
            token: PendingToken(1),
            result: Ok(HomeSnapshot::new(workspace, vec![session])),
        });
        run_new_fake_cycle(&mut state, &mut backend, effects);
        assert_eq!(backend.effects().len(), 1);
        assert_eq!(state.pending(), None);
        assert_eq!(state.progress(), None);
        assert!(matches!(
            state.route(),
            NewRoute::Home(home) if home.workspace() == workspace && home.sessions() == [session]
        ));
    }

    #[test]
    fn new_submit_while_pending_ignores_the_duplicate_operation() {
        let mut state = NewState::new(NewMode::Clone, clone_form());
        let first = update_new(&mut state, NewEvent::Submit);
        assert_eq!(first.len(), 1);
        assert_eq!(state.pending(), Some(PendingToken(1)));

        // A second Submit before the backend completes is a no-op: it produces
        // no new effect and does not advance the pending token, so a fast double
        // Enter cannot start two clones.
        let second = update_new(&mut state, NewEvent::Submit);
        assert!(second.is_empty());
        assert_eq!(state.pending(), Some(PendingToken(1)));

        // Retry is guarded the same way while an operation is in flight.
        assert!(update_new(&mut state, NewEvent::Retry).is_empty());
        assert_eq!(state.pending(), Some(PendingToken(1)));
    }

    #[test]
    fn new_existing_failure_retains_form_and_retry_reuses_the_request() {
        let mut state = NewState::new(NewMode::Existing, existing_form());
        let effects = update_new(&mut state, NewEvent::Submit);
        let expected = Effect::RegisterWorkspace {
            path: PathBuf::from("/work/existing"),
            name: "existing".to_owned(),
            token: PendingToken(1),
        };
        assert_eq!(effects, vec![expected]);
        let _ = update_new(
            &mut state,
            NewEvent::Result {
                token: PendingToken(1),
                result: Err(Notice::new("directory is not a project")),
            },
        );
        assert!(matches!(state.route(), NewRoute::Form));
        assert_eq!(state.form(), &existing_form());
        assert_eq!(
            state.error().map(|notice| notice.message.as_str()),
            Some("directory is not a project")
        );
        assert_eq!(state.progress(), None);

        assert_eq!(
            update_new(&mut state, NewEvent::Retry),
            vec![Effect::RegisterWorkspace {
                path: PathBuf::from("/work/existing"),
                name: "existing".to_owned(),
                token: PendingToken(2),
            }]
        );
        assert_eq!(
            state.progress().map(SafeMessage::as_str),
            Some("Registering workspace…")
        );
    }

    #[test]
    fn new_validation_and_late_completion_keep_the_form_route() {
        let mut invalid = NewState::new(NewMode::Clone, NewForm::default());
        assert!(update_new(&mut invalid, NewEvent::Submit).is_empty());
        assert_eq!(
            invalid.error().map(|notice| notice.message.as_str()),
            Some("repository URL is required")
        );

        let mut state = NewState::new(NewMode::Existing, existing_form());
        let _ = update_new(&mut state, NewEvent::Submit);
        let _ = update_new(
            &mut state,
            NewEvent::Result {
                token: PendingToken(99),
                result: Err(Notice::new("late failure")),
            },
        );
        assert_eq!(state.pending(), Some(PendingToken(1)));
        assert!(matches!(state.route(), NewRoute::Form));
        assert_eq!(state.error(), None);
    }

    #[test]
    fn new_validation_reports_every_required_clone_and_existing_field() {
        let cases = [
            (
                NewMode::Clone,
                NewForm::default(),
                NewValidationError::RepositoryRequired,
            ),
            (
                NewMode::Clone,
                NewForm {
                    repository: "repo".to_owned(),
                    ..NewForm::default()
                },
                NewValidationError::LocationRequired,
            ),
            (
                NewMode::Clone,
                NewForm {
                    repository: "repo".to_owned(),
                    location: "/work".to_owned(),
                    ..NewForm::default()
                },
                NewValidationError::DirectoryRequired,
            ),
            (
                NewMode::Existing,
                NewForm::default(),
                NewValidationError::PathRequired,
            ),
            (
                NewMode::Existing,
                NewForm {
                    path: "/work/existing".to_owned(),
                    ..NewForm::default()
                },
                NewValidationError::NameRequired,
            ),
        ];
        for (mode, form, expected) in cases {
            assert_eq!(validate_new_form(mode, &form), Err(expected));
            assert!(!expected.message().is_empty());
            assert!(!format!("{expected:?}").is_empty());
        }
    }

    #[test]
    fn table_driven_mode_and_overlay_scenarios() {
        struct Case {
            name: &'static str,
            events: Vec<AppEvent>,
            route: Route,
            overlay: Option<Overlay>,
        }
        let (workspace, first, _) = ids();
        let cases = [
            Case {
                name: "switch escape is no-op",
                events: vec![AppEvent::Key(AppKey::Escape)],
                route: Route::Home(HomeMode::Switch),
                overlay: None,
            },
            Case {
                name: "overview returns to switch origin",
                events: vec![
                    AppEvent::Key(AppKey::OpenOverview),
                    AppEvent::Key(AppKey::Escape),
                ],
                route: Route::Home(HomeMode::Switch),
                overlay: None,
            },
            Case {
                name: "closeup overlay escape returns to switch",
                events: vec![
                    AppEvent::LivePaneAvailability(true),
                    AppEvent::Key(AppKey::Enter),
                    AppEvent::Key(AppKey::OpenCloseupOverlay),
                    AppEvent::Key(AppKey::Escape),
                ],
                route: Route::Home(HomeMode::Switch),
                overlay: None,
            },
            Case {
                name: "closeup escape is no-op",
                events: vec![
                    AppEvent::LivePaneAvailability(true),
                    AppEvent::Key(AppKey::Enter),
                    AppEvent::Key(AppKey::Escape),
                ],
                route: Route::Home(HomeMode::Closeup),
                overlay: None,
            },
        ];
        for case in cases {
            let mut state = AppState::home(workspace, vec![first]);
            for event in case.events {
                let _ = update(&mut state, event);
            }
            assert_eq!(state.route(), case.route, "{}", case.name);
            assert_eq!(state.overlay(), case.overlay, "{}", case.name);
        }
    }

    #[test]
    fn switch_ctrl_c_is_ignored_while_closeup_preserves_existing_quit_behavior() {
        let (workspace, session, _) = ids();
        let mut idle = AppState::home(workspace, Vec::new());
        assert!(update(&mut idle, AppEvent::Key(AppKey::CtrlC)).is_empty());
        assert_eq!(idle.route(), Route::Home(HomeMode::Switch));
        assert_eq!(idle.overlay(), None);

        let mut live = AppState::home(workspace, vec![session]);
        let _ = update(&mut live, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut live, AppEvent::Key(AppKey::Enter));
        assert_eq!(live.route(), Route::Home(HomeMode::Closeup));
        assert!(live.has_live_pane());
        assert!(update(&mut live, AppEvent::Key(AppKey::CtrlC)).is_empty());
        assert_eq!(live.overlay(), Some(Overlay::QuitConfirmation));

        // Confirmation is deliberately immune to repeated quit chords.
        for key in [AppKey::CtrlC, AppKey::CtrlQ] {
            assert!(update(&mut live, AppEvent::Key(key)).is_empty());
            assert_eq!(live.overlay(), Some(Overlay::QuitConfirmation));
        }
        assert_eq!(
            update(&mut live, AppEvent::Key(AppKey::Char('Y'))),
            vec![Effect::Detach]
        );
        assert_eq!(live.overlay(), None);
    }

    #[test]
    fn management_ctrl_q_always_confirms_and_confirmation_can_cancel() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlQ)).is_empty());
        assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
        // Opening the confirmation focuses Yes by default.
        assert!(state.quit_confirm_selected());
        assert!(update(&mut state, AppEvent::Key(AppKey::Char('n'))).is_empty());
        assert_eq!(state.overlay(), None);

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenQuitConfirmation));
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Enter)),
            vec![Effect::Detach]
        );
    }

    #[test]
    fn quit_confirmation_focus_moves_and_enter_commits_the_selected_button() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());

        // ←→/Tab all toggle the two-button focus, and re-opening resets to Yes.
        for toggle in [AppKey::Left, AppKey::Right, AppKey::Tab] {
            let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
            assert!(state.quit_confirm_selected());
            assert!(update(&mut state, AppEvent::Key(toggle)).is_empty());
            assert!(!state.quit_confirm_selected());
            assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
            // Enter on the No button stays instead of detaching.
            assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
            assert_eq!(state.overlay(), None);
        }

        // With No focused, `y` still forces detach and `n`/Esc still stay.
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
        let _ = update(&mut state, AppEvent::Key(AppKey::Left));
        assert!(!state.quit_confirm_selected());
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Char('y'))),
            vec![Effect::Detach]
        );
        assert_eq!(state.overlay(), None);

        // Toggling back to Yes then Enter detaches.
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
        let _ = update(&mut state, AppEvent::Key(AppKey::Left));
        let _ = update(&mut state, AppEvent::Key(AppKey::Right));
        assert!(state.quit_confirm_selected());
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Enter)),
            vec![Effect::Detach]
        );
    }

    #[test]
    fn arrow_keys_are_inert_outside_the_quit_confirmation() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);
        let before = state.selected();
        assert!(update(&mut state, AppEvent::Key(AppKey::Left)).is_empty());
        assert!(update(&mut state, AppEvent::Key(AppKey::Right)).is_empty());
        assert_eq!(state.selected(), before);
        assert_eq!(state.overlay(), None);
    }

    #[test]
    fn switch_ctrl_c_never_detaches_after_leaving_a_live_pane() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
        assert!(state.ctrl_c_grace());
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
        assert!(state.ctrl_c_grace());

        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert!(!state.ctrl_c_grace());
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
    }

    #[test]
    fn live_pane_availability_reacts_on_the_edge_not_the_level() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        assert!(state.has_live_pane());
        assert_eq!(state.overlay(), None);

        // A quit confirmation over the live pane survives a re-sampled, unchanged
        // live level (the runtime resamples on every event).
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlC));
        assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));

        // Leaving the pane arms the grace once; a repeated non-live level keeps it.
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('n')));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
        assert!(state.ctrl_c_grace());
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
        assert!(state.ctrl_c_grace());
    }

    #[test]
    fn ordinary_modals_keep_ctrl_c_and_ctrl_q_inert() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        // The Overview palette (unlike the Closeup action modal) swallows both
        // quit chords and only leaves on Escape.
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let expected = state.overlay();
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
        assert_eq!(state.overlay(), expected);
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlQ)).is_empty());
        assert_eq!(state.overlay(), expected);
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
    }

    /// #355: the Closeup action modal is not an ordinary modal — Escape and
    /// Ctrl-C both close it and return Home to Switch, while Ctrl-Q stays inert
    /// like every other overlay.
    #[test]
    fn closeup_action_modal_exits_to_switch_on_escape_and_ctrl_c() {
        let (workspace, session, _) = ids();
        for exit_key in [AppKey::Escape, AppKey::CtrlC] {
            // Enter Closeup on a session with no live pane: the action modal is
            // the base surface.
            let mut state = AppState::home(workspace, vec![session]);
            let _ = update(&mut state, AppEvent::Key(AppKey::Down));
            let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
            assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
            assert_eq!(state.overlay(), Some(Overlay::Closeup));

            // Ctrl-Q keeps the modal, matching the other overlays' swallow.
            assert!(update(&mut state, AppEvent::Key(AppKey::CtrlQ)).is_empty());
            assert_eq!(state.overlay(), Some(Overlay::Closeup));

            // The exit key closes the modal and lands on Switch.
            assert!(update(&mut state, AppEvent::Key(exit_key.clone())).is_empty());
            assert_eq!(state.route(), Route::Home(HomeMode::Switch), "{exit_key:?}");
            assert_eq!(state.overlay(), None, "{exit_key:?}");
        }
    }

    /// #355: even when the action modal is forced over a live pane, Escape and
    /// Ctrl-C leave to Switch rather than handing input back to the live pane,
    /// and a trailing live resample does not resurrect the overlay.
    #[test]
    fn closeup_forced_action_modal_exits_to_switch() {
        let (workspace, session, _) = ids();
        for exit_key in [AppKey::Escape, AppKey::CtrlC] {
            let mut state = AppState::home(workspace, vec![session]);
            let _ = update(&mut state, AppEvent::Key(AppKey::Down));
            let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
            let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
            assert!(state.has_live_pane());
            assert_eq!(state.overlay(), None);

            // Force the action modal over the live pane, then exit it.
            let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
            assert_eq!(state.overlay(), Some(Overlay::Closeup));
            assert!(update(&mut state, AppEvent::Key(exit_key.clone())).is_empty());
            assert_eq!(state.route(), Route::Home(HomeMode::Switch), "{exit_key:?}");
            assert_eq!(state.overlay(), None, "{exit_key:?}");

            // A same-level live resample must not re-open the Closeup overlay now
            // that the route is Switch.
            let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
            assert_eq!(state.overlay(), None, "{exit_key:?}");
        }
    }

    #[test]
    fn cursor_moves_without_changing_active_target() {
        let (workspace, first, second) = ids();
        let mut state = AppState::home(workspace, vec![first, second]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(state.selected(), Selection::Target(Target::Session(first)));
        assert_eq!(state.active(), Target::Root(workspace));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.active(), Target::Session(first));
    }

    #[test]
    fn ctrl_a_opens_a_typed_create_form_and_lands_only_without_later_interaction() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
        assert_eq!(state.active(), Target::Root(workspace));
        assert_eq!(state.selected(), Selection::NewSession);
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        assert_eq!(state.create_session_form().unwrap().name(), "");
        // Home / Tab while the name-only form owns input must not retrigger create
        // nor edit any removed field.
        let _ = update(&mut state, AppEvent::Key(AppKey::Home));
        let _ = update(&mut state, AppEvent::Key(AppKey::Tab));
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        for key in [
            AppKey::Char('w'),
            AppKey::Char('o'),
            AppKey::Char('r'),
            AppKey::Char('k'),
        ] {
            let _ = update(&mut state, AppEvent::Key(key));
        }
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert!(matches!(
            &effects[..],
            [Effect::CreateSession { workspace: actual_workspace, token: PendingToken(1), intent, .. }]
                if *actual_workspace == workspace
                    && intent.name == "work"
                    && intent.profile.is_none()
                    && intent.model.is_none()
        ));
        assert_eq!(state.pending().len(), 1);
        let token = state.pending()[0].token;

        let created = SessionId::new();
        assert!(
            update(
                &mut state,
                AppEvent::OperationResult(OperationResult {
                    token,
                    succeeded: true,
                    created: Some(created),
                    notice: Some(Notice::new("created")),
                }),
            )
            .is_empty()
        );
        assert!(state.pending().is_empty());
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("created")
        );
        assert_eq!(state.active(), Target::Session(created));
        assert_eq!(
            state.selected(),
            Selection::Target(Target::Session(created))
        );

        let effects = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token: PendingToken(99),
                succeeded: false,
                created: None,
                notice: Some(Notice::new("safe failure")),
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("safe failure")
        );
    }

    /// Drive a create to a submitted request and return its pending token.
    #[coverage(off)]
    fn submit_create(state: &mut AppState, name: &[char]) -> PendingToken {
        let _ = update(state, AppEvent::Key(AppKey::CtrlA));
        for character in name {
            let _ = update(state, AppEvent::Key(AppKey::Char(*character)));
        }
        match &update(state, AppEvent::Key(AppKey::Enter))[..] {
            [Effect::CreateSession { token, .. }] => *token,
            other => panic!("expected a single create effect, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_create_opens_the_error_dialog_with_only_the_safe_message() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let token = submit_create(&mut state, &['a', 'p', 'i']);
        // Submitting closes the form and leaves no overlay open.
        assert_eq!(state.overlay(), None);
        assert!(state.create_session_form().is_none());
        assert_eq!(state.pending().len(), 1);

        let effects = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new("worktree path already exists")),
            }),
        );
        assert!(effects.is_empty());
        // The pending row is cleared and the dialog carries the safe message.
        assert!(state.pending().is_empty());
        assert_eq!(state.overlay(), Some(Overlay::CreateSessionError));
        assert_eq!(
            state
                .create_session_error()
                .map(|notice| notice.message.as_str()),
            Some("worktree path already exists")
        );
        // No half-created state leaks: sidebar rows and active target are unchanged.
        assert!(state.sessions().is_empty());
        assert_eq!(state.active(), Target::Root(workspace));
    }

    #[test]
    fn dismissing_the_create_error_dialog_returns_to_home_without_residue() {
        let (workspace, _, _) = ids();
        for dismiss in [AppKey::Escape, AppKey::Enter, AppKey::CtrlC] {
            let mut state = AppState::home(workspace, Vec::new());
            let token = submit_create(&mut state, &['x']);
            let _ = update(
                &mut state,
                AppEvent::OperationResult(OperationResult {
                    token,
                    succeeded: false,
                    created: None,
                    notice: Some(Notice::new("daemon unavailable")),
                }),
            );
            assert_eq!(state.overlay(), Some(Overlay::CreateSessionError));

            let effects = update(&mut state, AppEvent::Key(dismiss));
            assert!(effects.is_empty());
            assert_eq!(state.overlay(), None);
            assert!(state.create_session_error().is_none());
            assert!(state.create_session_form().is_none());
            // Dismissal leaves the resident Home route intact.
            assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        }
    }

    #[test]
    fn a_create_failure_keeps_the_notice_fallback_while_another_overlay_is_open() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let token = submit_create(&mut state, &['y']);
        // The user opens the quit confirmation before the create result returns.
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlQ));
        assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));

        let _ = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new("safe failure")),
            }),
        );
        // The open overlay is not clobbered; the message stays a plain notice.
        assert_eq!(state.overlay(), Some(Overlay::QuitConfirmation));
        assert!(state.create_session_error().is_none());
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("safe failure")
        );
    }

    #[test]
    fn closeup_pane_navigation_chords_keep_create_and_action_scopes_separate() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);

        // Switch keeps Ctrl-A as the IME-safe create shortcut and ignores Ctrl-O.
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlO)).is_empty());
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));

        // On an active session in Closeup, Ctrl-A owns the target action surface,
        // and must not resurrect the workspace-level create form.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.route(), Route::Home(HomeMode::Closeup));
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
        assert!(state.create_session_form().is_none());

        // Ctrl-O is the Closeup-to-Switch pane-navigation transition, and it
        // clears the forced action overlay on the way out.
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlO)).is_empty());
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        assert_eq!(state.overlay(), None);
    }

    #[test]
    #[coverage(off)]
    fn invalid_create_stays_open_and_late_success_does_not_move_after_interaction() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::CtrlA));
        assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        assert_eq!(
            state
                .create_session_form()
                .and_then(CreateSessionForm::error)
                .map(|error| error.message.as_str()),
            Some("session name is required")
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('a')));
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        let token = match &effects[..] {
            [Effect::CreateSession { token, .. }] => *token,
            _ => panic!("expected create effect"),
        };
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        let created = SessionId::new();
        let _ = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token,
                succeeded: true,
                created: Some(created),
                notice: None,
            }),
        );
        assert_eq!(state.active(), Target::Root(workspace));
        assert_ne!(
            state.selected(),
            Selection::Target(Target::Session(created))
        );
    }

    #[test]
    fn fake_backend_records_effects_and_replays_events() {
        let (workspace, first, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let mut backend = FakeBackend::default();
        backend.push_event(BackendEvent::Sessions(vec![first]));
        run_fake_cycle(
            &mut state,
            &mut backend,
            vec![Effect::RefreshSessions { workspace }],
        );
        assert_eq!(backend.effects(), &[Effect::RefreshSessions { workspace }]);
        assert_eq!(state.sessions(), &[first]);
        assert_eq!(
            backend.take_effects(),
            vec![Effect::RefreshSessions { workspace }]
        );
        assert!(backend.effects().is_empty());
    }

    #[test]
    fn snapshot_falls_back_missing_selected_and_active_sessions_to_root() {
        let (workspace, first, second) = ids();
        let mut state = AppState::home(workspace, vec![first, second]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(vec![second])),
        );
        assert_eq!(state.selected(), Selection::Target(Target::Root(workspace)));
        assert_eq!(state.active(), Target::Root(workspace));
    }

    #[test]
    fn future_events_update_only_their_local_state() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(
            &mut state,
            AppEvent::Input(LiveInput::Paste(b"paste".to_vec())),
        );
        let _ = update(
            &mut state,
            AppEvent::Resize {
                width: 100,
                height: 40,
            },
        );
        let _ = update(&mut state, AppEvent::Tick);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Notice(Notice::new("connected"))),
        );
        assert_eq!(state.size(), Some((100, 40)));
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("connected")
        );
    }

    #[test]
    fn runtime_stream_converts_to_controller_events() {
        let notice = Notice::new("connected");
        let cases = [
            (
                RuntimeEvent::Input(LiveInput::Paste(b"paste".to_vec())),
                AppEvent::Input(LiveInput::Paste(b"paste".to_vec())),
            ),
            (
                RuntimeEvent::Resize {
                    width: 100,
                    height: 40,
                },
                AppEvent::Resize {
                    width: 100,
                    height: 40,
                },
            ),
            (RuntimeEvent::Tick, AppEvent::Tick),
            (
                RuntimeEvent::Backend(BackendEvent::Notice(notice.clone())),
                AppEvent::Backend(BackendEvent::Notice(notice)),
            ),
        ];

        for (runtime, expected) in cases {
            assert_eq!(AppEvent::from(runtime), expected);
        }
    }

    #[test]
    fn phase_projection_isolated_per_runtime_and_uses_the_documented_rank() {
        let (workspace, first, second) = ids();
        let mut state = AppState::home(workspace, vec![first, second]);
        let first_a = runtime(workspace, first);
        let first_b = runtime(workspace, first);
        let second_runtime = runtime(workspace, second);

        for (runtime, phase) in [
            (first_a.clone(), AgentPhase::Running),
            (first_b.clone(), AgentPhase::Waiting),
            (second_runtime.clone(), AgentPhase::Ready),
        ] {
            let _ = update(
                &mut state,
                AppEvent::Backend(BackendEvent::RuntimePhase { runtime, phase }),
            );
        }
        assert_eq!(state.runtimes().len(), 3);
        assert_eq!(
            state.phase_for(Target::Session(first)),
            TargetPhase::Waiting
        );
        assert_eq!(state.phase_for(Target::Session(second)), TargetPhase::Ready);

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: first_a,
                phase: AgentPhase::Ended,
            }),
        );
        assert_eq!(state.runtimes().len(), 3);
        assert_eq!(state.phase_for(Target::Session(first)), TargetPhase::Done);

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: second_runtime,
                phase: AgentPhase::Exited,
            }),
        );
        assert_eq!(state.phase_for(Target::Session(second)), TargetPhase::Done);
        assert_eq!(
            state.phase_for(Target::Root(workspace)),
            TargetPhase::Absent
        );
    }

    #[test]
    fn phase_projection_rejects_other_workspaces_and_removed_sessions() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);
        let foreign = runtime(WorkspaceId::new(), session);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: foreign,
                phase: AgentPhase::Running,
            }),
        );
        assert!(state.runtimes().is_empty());

        let known = runtime(workspace, session);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::RuntimePhase {
                runtime: known,
                phase: AgentPhase::Running,
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(Vec::new())),
        );
        assert!(state.runtimes().is_empty());
    }

    #[test]
    fn feedback_keeps_only_safe_message_and_error_id() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let error = SafeError {
            message: SafeMessage::new("Could not start terminal"),
            error_id: "err-42".to_string(),
        };
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::TerminalError(
                error.clone(),
            ))),
        );
        assert_eq!(state.feedback(), Some(&Feedback::TerminalError(error)));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Feedback(Feedback::Disconnected)),
        );
        assert_eq!(state.feedback(), Some(&Feedback::Disconnected));
    }

    #[test]
    fn navigation_wraps_up_and_ignores_non_command_characters() {
        let (workspace, first, _) = ids();
        let mut state = AppState::home(workspace, vec![first]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        assert_eq!(state.selected(), Selection::NewSession);
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
        assert_eq!(state.selected(), Selection::NewSession);
    }

    #[test]
    fn switch_x_removes_the_selected_session_and_shift_x_forces_it() {
        let (workspace, first, second) = ids();
        let mut state = AppState::home(workspace, vec![first, second]);

        // The root is never a removal target.
        assert!(update(&mut state, AppEvent::Key(AppKey::Char('x'))).is_empty());

        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Char('x'))),
            vec![Effect::RemoveSession {
                workspace,
                session: first,
                force: false,
            }]
        );
        assert_eq!(state.selected(), Selection::Target(Target::Root(workspace)));

        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Char('X'))),
            vec![Effect::RemoveSession {
                workspace,
                session: second,
                force: true,
            }]
        );
        assert_eq!(state.selected(), Selection::Target(Target::Session(first)));
    }

    #[test]
    fn modal_registry_dispatches_once_and_rejects_invalid_root_and_repeated_requests() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let effects = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("issue list".to_owned())),
        );
        assert_eq!(
            effects,
            vec![Effect::WorkspaceCommand {
                workspace,
                command: overview::Command::Issue {
                    arguments: "list".to_owned(),
                },
            }]
        );
        assert_eq!(state.overlay(), None);
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitOverview("issue list".to_owned())),
            )
            .is_empty()
        );

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitOverview("unknown".to_owned())),
            )
            .is_empty()
        );
        assert_eq!(state.overlay(), Some(Overlay::Overview));

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitCloseup("close".to_owned())),
            )
            .is_empty()
        );
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("workspace root cannot be closed")
        );

        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        let effects = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("terminal open".to_owned())),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::OpenTerminal { target: Target::Session(actual), arguments, .. }]
                if *actual == session && arguments == "open"
        ));
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitCloseup("terminal open".to_owned())),
            )
            .is_empty()
        );
    }

    #[test]
    fn workspace_root_active_starts_a_session_less_agent_and_terminal() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);
        // The workspace root is selected and active by default.
        assert_eq!(state.active, Target::Root(workspace));
        assert_eq!(Target::Root(workspace).session_id(), None);
        assert_eq!(Target::Session(session).session_id(), Some(session));

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        let agent = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("agent".to_owned())),
        );
        assert!(matches!(
            agent.as_slice(),
            [Effect::LaunchAgent { workspace: actual, session: None, profile: None, .. }]
                if *actual == workspace
        ));

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        let terminal = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("terminal open".to_owned())),
        );
        assert!(matches!(
            terminal.as_slice(),
            [Effect::OpenTerminal { target: Target::Root(actual), arguments, .. }]
                if *actual == workspace && arguments == "open"
        ));
    }

    #[test]
    fn overview_session_commands_use_typed_lifecycle_effects() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let create = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("session create feature-x".into())),
        );
        assert!(matches!(
            &create[..],
            [Effect::CreateSession { workspace: actual, intent, .. }]
                if *actual == workspace && intent.name == "feature-x" && intent.profile.is_none() && intent.model.is_none()
        ));
        assert_eq!(state.overlay(), None);

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        assert_eq!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitOverview("session list".into())),
            ),
            vec![Effect::RefreshSessions { workspace }]
        );

        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        assert_eq!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitOverview(
                    "session remove feature-x --force".into(),
                )),
            ),
            Vec::new()
        );
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("named session removal is available in the live TUI")
        );
    }

    #[test]
    #[coverage(off)]
    fn closeup_registry_dispatches_agent_and_validated_session_remove() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        let effects = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitCloseup("agent codex".to_owned())),
        );
        assert!(matches!(
            effects.as_slice(),
            [Effect::LaunchAgent {
                workspace: effect_workspace,
                session: effect_session,
                profile: Some(profile),
                ..
            }] if *effect_workspace == workspace && *effect_session == Some(session) && profile.as_str() == "codex"
        ));

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        assert_eq!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitCloseup("close --force".to_owned())),
            ),
            vec![Effect::RemoveSession {
                workspace,
                session,
                force: true,
            }]
        );

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitCloseup("chat".to_owned())),
            )
            .is_empty()
        );
        assert_eq!(state.overlay(), Some(Overlay::Closeup));
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("unknown closeup command: \"chat\"")
        );
    }

    #[test]
    #[coverage(off)]
    fn entry_open_single_preserves_the_selected_identity_into_home() {
        let first = WorkspaceId::new();
        let chosen = WorkspaceId::new();
        let session = SessionId::new();
        let mut state = EntryState::new(
            vec![
                EntryWorkspace::new(first, "renamed later"),
                EntryWorkspace::new(chosen, "selected"),
            ],
            Vec::new(),
        );

        assert!(update_entry(&mut state, EntryEvent::ShowOpen).is_empty());
        assert_eq!(
            update_entry(&mut state, EntryEvent::OpenSingle(chosen)),
            vec![Effect::AttachWorkspace { workspace: chosen }]
        );
        let _ = update_entry(
            &mut state,
            EntryEvent::AttachResult {
                workspace: chosen,
                result: Ok(HomeSnapshot::new(chosen, vec![session])),
            },
        );

        let EntryRoute::Home(home) = state.route() else {
            panic!("selected workspace should enter Home");
        };
        assert_eq!(home.workspace(), chosen);
        assert_eq!(home.sessions(), &[session]);
        assert_eq!(home.selected(), Selection::Target(Target::Root(chosen)));
    }

    #[test]
    fn entry_recent_uses_its_identity_and_ignores_stale_completion() {
        let recent = WorkspaceId::new();
        let delayed_workspace = WorkspaceId::new();
        let mut state = EntryState::new(Vec::new(), vec![recent]);

        assert_eq!(
            update_entry(&mut state, EntryEvent::OpenRecent(recent)),
            vec![Effect::AttachWorkspace { workspace: recent }]
        );
        let _ = update_entry(
            &mut state,
            EntryEvent::AttachResult {
                workspace: delayed_workspace,
                result: Ok(HomeSnapshot::new(delayed_workspace, Vec::new())),
            },
        );
        assert_eq!(state.route(), &EntryRoute::Welcome);
        assert_eq!(state.opening(), Some(recent));
        assert!(state.error().is_none());

        let _ = update_entry(
            &mut state,
            EntryEvent::AttachResult {
                workspace: recent,
                result: Ok(HomeSnapshot::new(recent, Vec::new())),
            },
        );
        assert!(matches!(state.route(), EntryRoute::Home(home) if home.workspace() == recent));
    }

    #[test]
    fn fake_entry_backend_replays_error_then_retry_without_opening_another_workspace() {
        let requested = WorkspaceId::new();
        let other = WorkspaceId::new();
        let mut state = EntryState::new(Vec::new(), vec![requested]);
        let mut backend = FakeEntryBackend::default();
        backend.push_event(EntryEvent::AttachResult {
            workspace: other,
            result: Ok(HomeSnapshot::new(other, Vec::new())),
        });
        backend.push_event(EntryEvent::AttachResult {
            workspace: requested,
            result: Err(Notice::new("temporary attach failure")),
        });

        let effects = update_entry(&mut state, EntryEvent::OpenRecent(requested));
        run_entry_fake_cycle(&mut state, &mut backend, effects);
        assert_eq!(state.route(), &EntryRoute::Welcome);
        assert_eq!(
            state.error().map(|notice| notice.message.as_str()),
            Some("temporary attach failure")
        );

        let retry = update_entry(&mut state, EntryEvent::Retry);
        run_entry_fake_cycle(&mut state, &mut backend, retry);
        assert_eq!(
            backend.effects(),
            &[
                Effect::AttachWorkspace {
                    workspace: requested
                },
                Effect::AttachWorkspace {
                    workspace: requested
                }
            ]
        );
        assert_eq!(state.opening(), Some(requested));
        assert_eq!(state.route(), &EntryRoute::Welcome);
    }

    #[test]
    fn entry_empty_open_and_unknown_recent_are_noops() {
        let unknown = WorkspaceId::new();
        let mut state = EntryState::new(Vec::new(), Vec::new());
        let _ = update_entry(&mut state, EntryEvent::ShowOpen);

        assert!(update_entry(&mut state, EntryEvent::OpenSingle(unknown)).is_empty());
        assert!(update_entry(&mut state, EntryEvent::Back).is_empty());
        assert_eq!(state.route(), &EntryRoute::Welcome);
        assert!(update_entry(&mut state, EntryEvent::OpenRecent(unknown)).is_empty());
    }

    #[test]
    fn entry_open_error_stays_on_its_screen_and_retries_the_same_identity() {
        let workspace = WorkspaceId::new();
        let mut state = EntryState::new(
            vec![EntryWorkspace::new(workspace, "broken registration")],
            Vec::new(),
        );
        let _ = update_entry(&mut state, EntryEvent::ShowOpen);
        let _ = update_entry(&mut state, EntryEvent::OpenSingle(workspace));
        let _ = update_entry(
            &mut state,
            EntryEvent::AttachResult {
                workspace,
                result: Err(Notice::new("workspace is unavailable")),
            },
        );

        assert_eq!(state.route(), &EntryRoute::Open);
        assert_eq!(
            state.error().map(|notice| notice.message.as_str()),
            Some("workspace is unavailable")
        );
        assert_eq!(
            update_entry(&mut state, EntryEvent::Retry),
            vec![Effect::AttachWorkspace { workspace }]
        );
        assert_eq!(state.opening(), Some(workspace));
    }

    #[test]
    fn entry_rejects_a_snapshot_for_another_workspace_and_allows_retry() {
        let requested = WorkspaceId::new();
        let returned = WorkspaceId::new();
        let mut state = EntryState::new(Vec::new(), vec![requested]);
        let _ = update_entry(&mut state, EntryEvent::OpenRecent(requested));
        let _ = update_entry(
            &mut state,
            EntryEvent::AttachResult {
                workspace: requested,
                result: Ok(HomeSnapshot::new(returned, Vec::new())),
            },
        );

        assert_eq!(state.route(), &EntryRoute::Welcome);
        assert_eq!(
            state.error().map(|notice| notice.message.as_str()),
            Some("workspace changed while opening; retry")
        );
        assert_eq!(
            update_entry(&mut state, EntryEvent::Retry),
            vec![Effect::AttachWorkspace {
                workspace: requested
            }]
        );
    }

    #[test]
    fn fake_port_keeps_note_and_environment_edits_on_safe_failures() {
        let (workspace, session, _) = ids();
        let target = Target::Session(session);
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let mut backend = FakeBackend::default();

        let effects = update(&mut state, AppEvent::Key(AppKey::OpenNotes));
        assert_eq!(effects, vec![Effect::LoadNotes { target }]);
        backend.push_event(BackendEvent::NotesLoaded {
            target,
            scratchpad: Scratchpad {
                note: Some("before".to_owned()),
                todos: vec![usagi_core::domain::note::SessionTodo::new("test it")],
                decisions: Vec::new(),
            },
        });
        run_fake_cycle(&mut state, &mut backend, effects);
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SelectNoteSection(NoteSection::Todos)),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::ToggleTodo(0)));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SetNoteDraft("document it".to_owned())),
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::CommitNoteDraft));
        let queued_saves = update(&mut state, AppEvent::Key(AppKey::SaveNotes));
        assert!(
            matches!(&queued_saves[..], [Effect::SaveNotes { target: saved_target, scratchpad }] if *saved_target == target && scratchpad.todos.len() == 2 && scratchpad.todos[0].done)
        );
        backend.push_event(BackendEvent::NotesError {
            target,
            error: SafeError {
                message: SafeMessage::new("Could not save notes"),
                error_id: "safe-note-1".to_owned(),
            },
        });
        run_fake_cycle(&mut state, &mut backend, queued_saves);
        let note = state.note_editor().unwrap();
        assert_eq!(note.scratchpad().todos[1].text, "document it");
        assert_eq!(
            note.error().unwrap().message.as_str(),
            "Could not save notes"
        );

        let effects = update(&mut state, AppEvent::Key(AppKey::OpenEnvironment));
        assert_eq!(effects, vec![Effect::LoadEnvironment { target }]);
        backend.push_event(BackendEvent::EnvironmentLoaded {
            target,
            entries: vec![EnvironmentEntry {
                name: "MODE".to_owned(),
                value: "dev".to_owned(),
            }],
        });
        run_fake_cycle(&mut state, &mut backend, effects);
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SetEnvironment {
                name: "MODE".to_owned(),
                value: "test".to_owned(),
            }),
        );
        let saves = update(&mut state, AppEvent::Key(AppKey::SaveEnvironment));
        assert_eq!(
            saves,
            vec![Effect::SaveEnvironment {
                target,
                entries: vec![EnvironmentEntry {
                    name: "MODE".to_owned(),
                    value: "test".to_owned()
                }],
            }]
        );
        backend.push_event(BackendEvent::EnvironmentError {
            target,
            error: SafeError {
                message: SafeMessage::new("Could not save environment"),
                error_id: "safe-env-1".to_owned(),
            },
        });
        run_fake_cycle(&mut state, &mut backend, saves);
        let environment = state.environment_editor().unwrap();
        assert_eq!(environment.entries()[0].value, "test");
        assert_eq!(
            environment.error().unwrap().message.as_str(),
            "Could not save environment"
        );
    }

    fn pending_decision(workspace: WorkspaceId) -> UserDecision {
        UserDecision {
            decision_id: UserDecisionId::new(),
            owner: usagi_core::domain::user_decision::UserDecisionOwner {
                workspace_id: workspace,
                session_id: Some(SessionId::new()),
                caller: usagi_core::domain::agent::CallerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: usagi_core::domain::id::AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "Choose a path".into(),
            prompt: "Which path?".into(),
            options: vec![usagi_core::domain::user_decision::UserDecisionOption {
                id: "safe".into(),
                label: "Safe".into(),
                description: Some("Keeps current state".into()),
            }],
            allow_freeform: false,
            expires_at: None,
            idempotency_key: None,
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: chrono::Utc::now(),
            resolved_at: None,
        }
    }

    #[test]
    fn decisions_are_workspace_fenced_retryable_and_removed_only_on_confirmation() {
        let workspace = WorkspaceId::new();
        let foreign = WorkspaceId::new();
        let decision = pending_decision(workspace);
        let mut state = AppState::home(workspace, Vec::new());
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::OpenDecisions)),
            vec![Effect::RefreshDecisions { workspace }]
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace: foreign,
                decisions: vec![pending_decision(foreign)],
            }),
        );
        assert!(state.decisions().is_empty());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![decision.clone()],
            }),
        );
        assert_eq!(state.unread_decision_ids().len(), 1);
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenDecisions));
        assert!(state.unread_decision_ids().is_empty());
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::SubmitDecision)),
            vec![Effect::ResolveDecision {
                workspace,
                decision_id: decision.decision_id,
                answer: UserDecisionAnswer::Option {
                    option_id: "safe".into()
                }
            }]
        );
        assert_eq!(state.decisions(), std::slice::from_ref(&decision));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::DecisionError {
                workspace,
                decision_id: decision.decision_id,
                error: SafeError {
                    message: SafeMessage::new("try again"),
                    error_id: "resolve".into(),
                },
            }),
        );
        assert_eq!(state.decisions(), std::slice::from_ref(&decision));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::DecisionResolved {
                workspace,
                decision_id: decision.decision_id,
            }),
        );
        assert!(state.decisions().is_empty());
    }

    fn pr_link(number: u32) -> PrLink {
        PrLink::new(number, format!("https://github.com/o/r/pull/{number}"))
    }

    fn safe_error(message: &str) -> SafeError {
        SafeError {
            message: SafeMessage::new(message),
            error_id: "overlay".into(),
        }
    }

    #[test]
    fn pr_overlay_opens_reflows_material_navigates_opens_and_closes() {
        let (workspace, session, _) = ids();
        let root = Target::Root(workspace);
        let mut state = AppState::home(workspace, vec![session]);

        // `p` opens the PR overlay for the active target and requests its list.
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Char('p'))),
            vec![Effect::LoadPullRequests { target: root }]
        );
        assert_eq!(state.overlay(), Some(Overlay::Prs));
        assert!(state.pr_overlay().unwrap().prs().is_empty());

        // A list for another target is ignored; the matching one fills the overlay.
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target: Target::Session(session),
                prs: vec![pr_link(9)],
            }),
        );
        assert!(state.pr_overlay().unwrap().prs().is_empty());
        let prs = vec![pr_link(1), pr_link(2)];
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsLoaded {
                target: root,
                prs: prs.clone(),
            }),
        );
        assert_eq!(state.pr_overlay().unwrap().prs().len(), 2);
        assert_eq!(state.pr_overlay().unwrap().selected(), 0);

        // Down/Up wrap around the list.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(state.pr_overlay().unwrap().selected(), 1);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(state.pr_overlay().unwrap().selected(), 0);
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        assert_eq!(state.pr_overlay().unwrap().selected(), 1);

        // Enter opens the selected PR through the browser effect.
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Enter)),
            vec![Effect::OpenPullRequest {
                url: prs[1].url.clone(),
            }]
        );

        // Esc closes the overlay and discards its state.
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        assert_eq!(state.overlay(), None);
        assert!(state.pr_overlay().is_none());
    }

    #[test]
    fn pr_overlay_enter_is_inert_while_empty_and_errors_stay_visible() {
        let (workspace, _, _) = ids();
        let root = Target::Root(workspace);
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
        // Enter with no entries emits nothing and keeps the overlay open.
        assert!(update(&mut state, AppEvent::Key(AppKey::Enter)).is_empty());
        assert_eq!(state.overlay(), Some(Overlay::Prs));
        // A safe fetch error surfaces on the open overlay.
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PullRequestsError {
                target: root,
                error: safe_error("gh unavailable"),
            }),
        );
        assert_eq!(
            state
                .pr_overlay()
                .unwrap()
                .error()
                .map(|error| error.message.as_str()),
            Some("gh unavailable")
        );
    }

    #[test]
    fn decision_snapshots_auto_open_only_for_new_pending_rows_without_stealing_an_overlay() {
        let workspace = WorkspaceId::new();
        let first = pending_decision(workspace);
        let second = pending_decision(workspace);
        let mut state = AppState::home(workspace, Vec::new());

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![first.clone()],
            }),
        );
        assert_eq!(state.overlay(), Some(Overlay::Decisions));
        assert!(state.decision_overlay().is_some());
        assert_eq!(state.unread_decision_ids().len(), 1);

        // Dismissal changes only UI state. A duplicate/resync snapshot must not
        // steal focus again, while a genuinely new pending row may notify.
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        assert_eq!(state.overlay(), None);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![first.clone()],
            }),
        );
        assert_eq!(state.overlay(), None);

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Decisions {
                workspace,
                decisions: vec![first, second],
            }),
        );
        assert_eq!(state.overlay(), Some(Overlay::Overview));
        assert_eq!(state.decisions().len(), 2);
    }

    #[test]
    fn preview_overlay_opens_reflows_scrolls_errors_and_closes() {
        let (workspace, session, _) = ids();
        let root = Target::Root(workspace);
        let mut state = AppState::home(workspace, vec![session]);

        // `v` opens the preview overlay for the active target and requests it.
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Char('v'))),
            vec![Effect::LoadPreview { target: root }]
        );
        assert_eq!(state.overlay(), Some(Overlay::Preview));

        // A preview for another target is ignored; the matching one fills it.
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target: Target::Session(session),
                lines: vec!["stale".into()],
            }),
        );
        assert!(state.preview_overlay().unwrap().lines().is_empty());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewLoaded {
                target: root,
                lines: vec!["# Title".into(), "body".into()],
            }),
        );
        assert_eq!(state.preview_overlay().unwrap().lines().len(), 2);

        // Down scrolls; Up saturates at the top.
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(state.preview_overlay().unwrap().scroll(), 1);
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        assert_eq!(state.preview_overlay().unwrap().scroll(), 0);

        // A safe read error surfaces on the open overlay.
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::PreviewError {
                target: root,
                error: safe_error("no preview"),
            }),
        );
        assert_eq!(
            state
                .preview_overlay()
                .unwrap()
                .error()
                .map(|error| error.message.as_str()),
            Some("no preview")
        );

        // Esc closes the overlay and discards its state.
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        assert_eq!(state.overlay(), None);
        assert!(state.preview_overlay().is_none());
    }

    #[test]
    fn opening_one_overlay_discards_the_other_state() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        // Open PRs, dismiss, then open preview: the PR state must not linger.
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
        assert!(state.pr_overlay().is_some());
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPreview));
        assert_eq!(state.overlay(), Some(Overlay::Preview));
        assert!(state.pr_overlay().is_none());
        assert!(state.preview_overlay().is_some());
        // And the reverse: opening PRs discards the preview state.
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenPrs));
        assert_eq!(state.overlay(), Some(Overlay::Prs));
        assert!(state.preview_overlay().is_none());
    }
}

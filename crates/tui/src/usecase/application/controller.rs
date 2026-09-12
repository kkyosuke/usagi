//! Home の純粋な application controller。
//!
//! [`update`] は TUI-local の [`AppEvent`] を受け、状態を更新して外部へ依頼する
//! [`Effect`] を返す。daemon の wire 型はここへ持ち込まない。実行側は
//! effect は backend adapter が固有の command に変換し、テストでは test-only
//! backend の command log と event queue を使う。

mod decision;
mod entry;
mod new;
mod preview;

pub use entry::{EntryEvent, EntryRoute, EntryState, EntryWorkspace, HomeSnapshot, update_entry};
pub use new::{
    NewEvent, NewForm, NewMode, NewRequest, NewRoute, NewState, NewValidationError, update_new,
    validate_new_form,
};
pub use preview::{PreviewFileFilter, PreviewOverlay, PreviewSearchMatch};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use usagi_core::domain::agent::{AgentProfileId, ModelSelector};
use usagi_core::domain::id::{
    AgentContinuationRef, AgentRuntimeId, AgentRuntimeRef, OperationId, RequestId, SessionId,
    UserDecisionId, WorkspaceId,
};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::pr_inventory::{PrEntry, PrState};
use usagi_core::domain::presentation_text::{
    presentation_character_is_safe, presentation_text_is_safe, sanitize_presentation_line,
};
use usagi_core::domain::role::RoleId;
use usagi_core::domain::session_lifecycle::{
    AgentPhase, FailureStage, SessionLifecycle, SessionLifecycleProjection,
};
use usagi_core::domain::settings::{
    AvailableModels, DefaultModel, EnvBindings, PrAutoOpen, WorkMode, format_env_bindings,
};
use usagi_core::domain::supervisor::SupervisorRunId;
use usagi_core::domain::user_decision::{UserDecision, UserDecisionAnswer, UserDecisionStatus};
use usagi_core::usecase::agent_phase::AgentPhaseAggregation;
use usagi_core::usecase::env::EnvScope;

use crate::usecase::application::environment_source::EnvironmentSourceEditor;
use crate::usecase::terminal_input::{
    KeyCode, KeyEventKind, LiveInput, RuntimeEvent, is_control_and_shift,
};
use crate::usecase::{agent_command, closeup, overview};

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
    /// daemon health / Agent capacity と安全な lifecycle 操作の surface。
    Daemon,
    /// active target scope の action surface。
    Closeup,
    /// Detach confirmation. This is TUI-local: confirming it never stops a
    /// daemon-owned terminal or operation.
    QuitConfirmation,
    /// Explicit Yes/No gate before retrying a failed deletion with force.
    ForceRemoveConfirmation,
    /// active target の note / todo / decision scratchpad。
    Notes,
    /// workspace または session の environment editor。
    Environment,
    /// Global/workspace versioned role catalog source editor.
    Roles,
    /// Home 左ペインの `+ new session` に対する入力。常駐 route ではない。
    CreateSession,
    /// Workspace-scoped pending user decisions and their answer editor.
    Decisions,
    /// Merge-confirmed sessions awaiting explicit, sequential cleanup.
    CleanupQueue,
    /// Explicit session-removal checklist opened from `session remove --select`.
    RemoveSessions,
    /// active target scope の Pull Request 一覧。素材は port から還流する。
    Prs,
    /// Selected session's repository file finder and read-only text preview.
    Preview,
    /// session 作成が accept 後に失敗したことを伝える dialog。表示は safe message だけ。
    CreateSessionError,
    /// terminal の起動要求が失敗したことを伝える dialog。表示は safe message だけ。
    TerminalLaunchError,
    /// Agent の起動要求が失敗したことを伝える dialog。表示は safe message だけ。
    AgentLaunchError,
    /// session を庭のうさぎとして眺める screen saver。読み取り専用で、最初の入力を
    /// wake-up として消費して Home へ戻る。
    Garden,
}

/// session name に許される最大文字数（表示・path 双方の実害を避ける上限）。
const MAX_SESSION_NAME_LEN: usize = 64;
/// Goal composer bound. The daemon repeats this limit before admitting work.
pub const MAX_WORK_GOAL_BYTES: usize = usagi_core::infrastructure::client::MAX_AGENT_GOAL_BYTES;

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
    roles: Vec<RoleChoice>,
    selected_role: Option<usize>,
    branches: Vec<BranchChoice>,
    selected_branch: Option<usize>,
}

/// One Git ref offered as a session base. The label is presentation-safe while
/// `refname` is the fully-qualified identity sent to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChoice {
    pub label: String,
    pub refname: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionBranchCatalog {
    pub branches: Vec<BranchChoice>,
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleChoice {
    pub id: RoleId,
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionRoleCatalog {
    pub roles: Vec<RoleChoice>,
    pub default: Option<RoleId>,
}

/// Safe daemon-owned role metadata, kept separate from persisted session annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoleProjection {
    pub role_id: Option<RoleId>,
    pub role_summary: Option<String>,
    pub parent_session_id: Option<SessionId>,
    pub agent_status: Option<usagi_core::domain::agent::AgentStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleEditorScope {
    Global,
    Workspace,
}

pub const ROLE_EDITOR_VIEWPORT_LINES: usize = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleEditor {
    scope: RoleEditorScope,
    source: String,
    scroll_top: usize,
    error: Option<SafeError>,
    loading: bool,
    saving: bool,
}

impl RoleEditor {
    fn loading(scope: RoleEditorScope) -> Self {
        Self {
            scope,
            source: String::new(),
            scroll_top: 0,
            error: None,
            loading: true,
            saving: false,
        }
    }
    #[must_use]
    pub const fn scope(&self) -> RoleEditorScope {
        self.scope
    }
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }
    #[must_use]
    pub const fn scroll_top(&self) -> usize {
        self.scroll_top
    }
    #[must_use]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        self.loading
    }
    #[must_use]
    pub const fn is_saving(&self) -> bool {
        self.saving
    }

    fn max_scroll_top(&self) -> usize {
        self.source
            .lines()
            .count()
            .saturating_sub(ROLE_EDITOR_VIEWPORT_LINES)
    }

    fn follow_tail(&mut self) {
        self.scroll_top = self.max_scroll_top();
    }

    fn scroll_up(&mut self, lines: usize) {
        self.scroll_top = self.scroll_top.saturating_sub(lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scroll_top = self
            .scroll_top
            .saturating_add(lines)
            .min(self.max_scroll_top());
    }
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
    pub fn with_catalogs(
        existing: Vec<String>,
        roles: &SessionRoleCatalog,
        branches: &SessionBranchCatalog,
    ) -> Self {
        let selected_role = roles
            .default
            .as_ref()
            .and_then(|default| roles.roles.iter().position(|role| &role.id == default));
        let selected_branch = branches
            .default
            .as_ref()
            .and_then(|default| {
                branches
                    .branches
                    .iter()
                    .position(|branch| &branch.refname == default)
            })
            .or_else(|| (!branches.branches.is_empty()).then_some(0));
        Self {
            existing,
            roles: roles.roles.clone(),
            selected_role,
            branches: branches.branches.clone(),
            selected_branch,
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

    #[must_use]
    pub fn roles(&self) -> &[RoleChoice] {
        &self.roles
    }

    #[must_use]
    pub fn selected_role(&self) -> Option<&RoleChoice> {
        self.selected_role.and_then(|index| self.roles.get(index))
    }

    #[must_use]
    pub fn selected_branch(&self) -> Option<&BranchChoice> {
        self.selected_branch
            .and_then(|index| self.branches.get(index))
    }

    fn move_branch(&mut self, backwards: bool) {
        if self.branches.is_empty() {
            self.selected_branch = None;
            return;
        }
        let current = self.selected_branch.unwrap_or(0);
        self.selected_branch = Some(if backwards {
            (current + self.branches.len() - 1) % self.branches.len()
        } else {
            (current + 1) % self.branches.len()
        });
    }

    fn move_role(&mut self, backwards: bool) {
        if self.roles.is_empty() {
            self.selected_role = None;
            return;
        }
        let current = self.selected_role.unwrap_or(0);
        self.selected_role = Some(if backwards {
            (current + self.roles.len() - 1) % self.roles.len()
        } else {
            (current + 1) % self.roles.len()
        });
    }

    fn push(&mut self, character: char) {
        self.name.push(character);
        self.revalidate();
    }

    fn paste(&mut self, text: &str) {
        self.name.push_str(text);
        self.revalidate();
    }

    fn backspace(&mut self) {
        self.name.pop();
        self.revalidate();
    }

    /// Replace the advisory session/worktree names while the form is open.
    ///
    /// A daemon refresh or the presentation layer's read-only worktree scan can
    /// discover a conflict after the user opened the inline input. Revalidate
    /// immediately so that newly known failures appear without another key
    /// press or a doomed submit.
    fn replace_existing(&mut self, existing: &[String]) {
        self.existing.clear();
        self.existing.extend_from_slice(existing);
        self.revalidate();
    }

    /// 入力のたびに name の live validation を反映する。空名は「入力途中」であって
    /// error にはせず（submit 時にだけ弾く）、不正文字・64 文字超過・既知 worktree との
    /// 同名は即座に
    /// 行の下の error として見せる。draft は決して失わない。
    fn revalidate(&mut self) {
        self.error = validate_session_name_live(&self.name, &self.existing);
    }

    /// submit（Enter）時の検証。空名はここで初めて error になる。
    fn request(&mut self) -> Result<SessionCreateIntent, Notice> {
        // 空名は submit 時にだけ弾く（入力途中は error にしない）。非空の name は
        // 不正文字・64 文字超過・既知 worktree との同名を local validation で拒否する。
        let name = required_create_value(&self.name, "session name is required")?;
        if let Some(error) = validate_session_name_live(&self.name, &self.existing) {
            return Err(error);
        }
        Ok(SessionCreateIntent {
            name,
            base_ref: self.selected_branch().map(|branch| branch.refname.clone()),
            profile: None,
            model: None,
            role_id: self.selected_role().map(|role| role.id.clone()),
        })
    }
}

/// 入力途中でも判定できる name の validation。空名（入力途中）は `None` を返し、
/// 不正文字・64 文字超過・既知 worktree との同名だけを safe な error にする。
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
        // `existing` is supplied from the daemon-authoritative sidebar
        // snapshot. A matching session name owns `.usagi/sessions/<name>`, so
        // surface this actionable conflict before a create request is sent.
        return Some(Notice::new("session name already exists"));
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
    /// Fully-qualified local or remote-tracking ref selected as the base.
    pub base_ref: Option<String>,
    pub profile: Option<AgentProfileId>,
    pub model: Option<ModelSelector>,
    /// Only the selector crosses to the daemon; definitions stay in `roles.toml`.
    pub role_id: Option<RoleId>,
}

fn required_create_value(value: &str, message: &str) -> Result<String, Notice> {
    let value = value.trim();
    (!value.is_empty())
        .then(|| value.to_owned())
        .ok_or_else(|| Notice::new(message))
}

/// The daemon profile ID for a selected CLI. The mapping lives in the core
/// settings vocabulary, so the TUI never spells a product profile itself.
fn profile_for(model: DefaultModel) -> AgentProfileId {
    AgentProfileId::new(model.profile_id()).expect("vocabulary profile ID is canonical")
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
    pub const fn target(&self) -> Target {
        self.target
    }
    /// 現在の表示・編集値。
    #[must_use]
    pub fn scratchpad(&self) -> &Scratchpad {
        &self.scratchpad
    }
    /// 選択された section。
    #[must_use]
    pub const fn section(&self) -> NoteSection {
        self.section
    }
    /// todo / decision 追加用、または note の編集値。
    #[must_use]
    pub fn draft(&self) -> &str {
        &self.draft
    }
    /// port が分類した安全なエラー。
    #[must_use]
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

/// Environment editor state for one settings scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentEditor {
    scope: EnvScope,
    entries: Vec<EnvironmentEntry>,
    /// Shared Config/Home multiline source editing state.
    source: EnvironmentSourceEditor,
    error: Option<SafeError>,
    /// `true` while the initial read is in flight, before any values have
    /// refluxed. Distinguishes "still loading" from "loaded, but empty".
    loading: bool,
    /// `true` while a save is in flight. Local edits and re-saves are ignored
    /// until the owning port refluxes, so a save can never be double-submitted.
    saving: bool,
}

/// Local navigation and draft state for a durable user decision.  The durable
/// record itself remains daemon-owned; dismissing this state never mutates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionEditor {
    decision: UserDecision,
    selected_option: usize,
    /// Explicit text viewport offset. `None` follows the active automatic anchor.
    scroll_offset: Option<usize>,
    /// Whether automatic scrolling follows the freeform draft instead.
    follow_freeform: bool,
    freeform: String,
    error: Option<SafeError>,
}

impl DecisionEditor {
    fn new(decision: UserDecision) -> Self {
        Self {
            decision,
            selected_option: 0,
            scroll_offset: None,
            follow_freeform: false,
            freeform: String::new(),
            error: None,
        }
    }
    #[must_use]
    pub fn decision(&self) -> &UserDecision {
        &self.decision
    }
    #[must_use]
    pub const fn selected_option(&self) -> usize {
        self.selected_option
    }
    #[must_use]
    pub const fn scroll_offset(&self) -> Option<usize> {
        self.scroll_offset
    }
    #[must_use]
    pub const fn follows_freeform(&self) -> bool {
        self.follow_freeform
    }
    #[must_use]
    pub fn freeform(&self) -> &str {
        &self.freeform
    }
    #[must_use]
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
    pub const fn selected(&self) -> usize {
        self.selected
    }
    #[must_use]
    pub fn editor(&self) -> Option<&DecisionEditor> {
        self.editor.as_ref()
    }
}

/// active target の Pull Request 一覧 overlay state。
///
/// 一覧 [`PrEntry`] は domain データで、素材は port（[`Effect::LoadPullRequests`]）から
/// [`BackendEvent::PullRequestsLoaded`] として還流する。reducer が所有するのは選択位置と
/// 表示可能なエラーだけで、URL の妥当性検証や browser 起動は executor 側に残す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrOverlay {
    target: Target,
    prs: Vec<PrEntry>,
    selected: usize,
    error: Option<SafeError>,
    filter: PrFilter,
}

/// Stable identities in the merge-confirmed cleanup queue.
///
/// Names and PR metadata remain live projections outside this state. Selection
/// can therefore survive redraws without a refreshed row rebinding to another
/// session, and every removal is revalidated immediately before dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupQueueState {
    candidates: Vec<SessionId>,
    selected: BTreeSet<SessionId>,
    cursor: usize,
    in_flight: Option<SessionId>,
    feedback: Option<Notice>,
}

/// Stable identities selected for explicit session removal.
///
/// This is separate from [`CleanupQueueState`]: cleanup admits only completed
/// sessions whose visible PRs are all merged, while the explicit selector lists
/// every lifecycle row that the daemon says can be removed. Both queues dispatch
/// one removal at a time so the workspace's bounded session-command lane never
/// receives concurrent mutations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveQueueState {
    candidates: Vec<SessionId>,
    selected: BTreeSet<SessionId>,
    cursor: usize,
    in_flight: Option<SessionId>,
    force: bool,
    feedback: Option<Notice>,
}

impl RemoveQueueState {
    fn new(candidates: Vec<SessionId>, cursor: usize, force: bool) -> Self {
        Self {
            cursor: cursor.min(candidates.len().saturating_sub(1)),
            candidates,
            selected: BTreeSet::new(),
            in_flight: None,
            force,
            feedback: None,
        }
    }

    #[must_use]
    pub fn candidates(&self) -> &[SessionId] {
        &self.candidates
    }

    #[must_use]
    pub fn selected(&self) -> &BTreeSet<SessionId> {
        &self.selected
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub const fn in_flight(&self) -> Option<SessionId> {
        self.in_flight
    }

    #[must_use]
    pub const fn force(&self) -> bool {
        self.force
    }

    #[must_use]
    pub fn feedback(&self) -> Option<&Notice> {
        self.feedback.as_ref()
    }
}

impl CleanupQueueState {
    fn new(candidates: Vec<SessionId>) -> Self {
        Self {
            candidates,
            selected: BTreeSet::new(),
            cursor: 0,
            in_flight: None,
            feedback: None,
        }
    }

    #[must_use]
    pub fn candidates(&self) -> &[SessionId] {
        &self.candidates
    }

    #[must_use]
    pub fn selected(&self) -> &BTreeSet<SessionId> {
        &self.selected
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub const fn in_flight(&self) -> Option<SessionId> {
        self.in_flight
    }

    #[must_use]
    pub fn feedback(&self) -> Option<&Notice> {
        self.feedback.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrFilter {
    #[default]
    All,
    Open,
    Closed,
    Merged,
}

impl PrFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Open,
            Self::Open => Self::Closed,
            Self::Closed => Self::Merged,
            Self::Merged => Self::All,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::All => Self::Merged,
            Self::Open => Self::All,
            Self::Closed => Self::Open,
            Self::Merged => Self::Closed,
        }
    }

    /// Status tabs in their horizontal navigation order.
    pub const TABS: [Self; 4] = [Self::All, Self::Open, Self::Closed, Self::Merged];

    #[must_use]
    pub const fn tab_index(self) -> usize {
        match self {
            Self::All => 0,
            Self::Open => 1,
            Self::Closed => 2,
            Self::Merged => 3,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }
}

fn filtered_prs(prs: &[PrEntry], filter: PrFilter) -> Vec<PrEntry> {
    prs.iter()
        .filter(|pr| {
            pr.state != PrState::Dismissed
                && match filter {
                    PrFilter::All => true,
                    PrFilter::Open => pr.state == PrState::Open,
                    PrFilter::Closed => pr.state == PrState::Closed,
                    PrFilter::Merged => pr.state == PrState::Merged,
                }
        })
        .cloned()
        .collect()
}

impl PrOverlay {
    fn loading(target: Target) -> Self {
        Self {
            target,
            prs: Vec::new(),
            selected: 0,
            error: None,
            filter: PrFilter::All,
        }
    }

    /// Overlay が対象とする stable identity。
    #[must_use]
    pub const fn target(&self) -> Target {
        self.target
    }
    /// 表示中の PR 一覧。素材未着なら空。
    #[must_use]
    pub fn prs(&self) -> &[PrEntry] {
        &self.prs
    }
    /// 選択中の添字。
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }
    /// 選択中の PR。一覧が空なら `None`。
    #[must_use]
    pub fn selected_pr(&self) -> Option<&PrEntry> {
        self.prs.get(self.selected)
    }
    /// port が分類した安全なエラー。
    #[must_use]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
    #[must_use]
    pub const fn filter(&self) -> PrFilter {
        self.filter
    }
}

impl EnvironmentEditor {
    fn loading(scope: EnvScope) -> Self {
        Self {
            scope,
            entries: Vec::new(),
            source: EnvironmentSourceEditor::default(),
            error: None,
            loading: true,
            saving: false,
        }
    }

    /// Which stored scope this editor reads and writes.
    #[must_use]
    pub const fn scope(&self) -> EnvScope {
        self.scope
    }
    #[must_use]
    pub fn entries(&self) -> &[EnvironmentEntry] {
        &self.entries
    }
    /// The `NAME=value` line being typed.
    #[must_use]
    pub fn draft(&self) -> &str {
        self.source.value()
    }
    /// Byte cursor in the multiline source.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.source.cursor()
    }
    /// Whether the multiline editor's Save action owns Enter.
    #[must_use]
    pub const fn is_save_focused(&self) -> bool {
        self.source.is_save_focused()
    }
    #[must_use]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
    /// Whether the initial read is still in flight (no values refluxed yet).
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        self.loading
    }
    /// Whether a save is in flight; the editor rejects edits and re-saves until
    /// it clears.
    #[must_use]
    pub const fn is_saving(&self) -> bool {
        self.saving
    }
    /// Whether the editor is accepting local edits and saves (neither the
    /// initial read nor a save is in flight).
    fn is_busy(&self) -> bool {
        self.loading || self.saving
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

/// Home の navigation cursor。action row と空 workspace の中立状態を区別する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// session が無い Home で、まだ action row を明示選択していない。
    Idle,
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
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Rebuild a token from its raw value. Only tests synthesize effects
    /// directly; the reducer is the sole producer at runtime.
    #[cfg(test)]
    #[must_use]
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

const MAX_PRESENTATION_MESSAGE_CHARS: usize = 240;

/// Normalize untrusted detail into a bounded, one-line terminal-safe summary.
///
/// Control characters become a single separating space and bidi controls
/// become a visible replacement character. This preserves useful wording
/// without allowing terminal control flow or visual reordering. The frame is a
/// second, fail-closed boundary for values which do not use these message types.
fn sanitize_presentation_message(message: &str) -> String {
    let mut sanitized = String::with_capacity(message.len().min(256));
    let mut character_count = 0;
    let mut previous_was_space = true;
    let mut truncated = false;

    for character in message.chars() {
        let character = if presentation_character_is_safe(character) {
            character
        } else if character.is_control() {
            ' '
        } else {
            '\u{fffd}'
        };
        if character == ' ' && previous_was_space {
            continue;
        }
        if character_count == MAX_PRESENTATION_MESSAGE_CHARS - 1 {
            truncated = true;
            break;
        }
        sanitized.push(character);
        character_count += 1;
        previous_was_space = character == ' ';
    }
    while sanitized.ends_with(' ') {
        sanitized.pop();
    }
    if truncated {
        sanitized.push('…');
    }
    sanitized
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
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            message: sanitize_presentation_message(&message),
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
    const fn rank(self) -> u8 {
        self.aggregation().rank()
    }

    /// The shared aggregation class of this projected phase, so a caller
    /// classifying a session reuses the core vocabulary instead of re-deriving
    /// its own mapping.
    #[must_use]
    pub const fn aggregation(self) -> AgentPhaseAggregation {
        match self {
            Self::Absent => AgentPhaseAggregation::Absent,
            Self::Ready => AgentPhaseAggregation::Ready,
            Self::Running => AgentPhaseAggregation::Running,
            Self::Waiting => AgentPhaseAggregation::Waiting,
            Self::Done => AgentPhaseAggregation::Done,
        }
    }

    fn from_agent_phase(phase: AgentPhase) -> Self {
        match AgentPhaseAggregation::from_phase(phase) {
            AgentPhaseAggregation::Absent => Self::Absent,
            AgentPhaseAggregation::Ready => Self::Ready,
            AgentPhaseAggregation::Running => Self::Running,
            AgentPhaseAggregation::Waiting => Self::Waiting,
            AgentPhaseAggregation::Done => Self::Done,
        }
    }
}

/// One runtime-local phase entry. The complete runtime reference is retained so
/// an update for one pane can never overwrite another pane in the same session.
///
/// The daemon's concrete [`AgentPhase`] is kept here instead of the session-level
/// [`TargetPhase`] fold. Callers which classify a whole target still fold through
/// [`TargetPhase::from_agent_phase`], while per-Agent surfaces (the sidebar Agent
/// row and Garden) keep `interrupted` distinct from `ended` / `exited`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePhase {
    pub runtime: AgentRuntimeRef,
    pub phase: AgentPhase,
}

/// A bounded, one-line message which is safe to show in a terminal.
///
/// Construction sanitizes defensively. Backend adapters must still map raw
/// diagnostic detail to stable user-facing summaries so filesystem paths and
/// credentials never become presentation content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeMessage(String);

impl SafeMessage {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        Self(sanitize_presentation_message(&message))
    }

    #[must_use]
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

/// Safe daemon process lifecycle operations exposed by the Home modal.
///
/// Destructive `--force` variants deliberately stay in the CLI. These actions
/// either preserve live runtimes or are refused by the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAction {
    Start,
    Restart,
    Stop,
}

impl DaemonAction {
    pub const ALL: [Self; 3] = [Self::Start, Self::Restart, Self::Stop];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Restart => "Restart",
            Self::Stop => "Stop",
        }
    }

    #[must_use]
    pub const fn argument(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Restart => "restart",
            Self::Stop => "stop",
        }
    }

    #[must_use]
    const fn shifted(self, direction: i8) -> Self {
        let current = match self {
            Self::Start => 0,
            Self::Restart => 1,
            Self::Stop => 2,
        };
        let next = if direction > 0 {
            (current + 1) % Self::ALL.len()
        } else {
            (current + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

/// Controller-owned interaction state for the daemon management modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonControlState {
    selected: DaemonAction,
    pending: Option<(DaemonAction, PendingToken)>,
    result: Option<Result<Notice, SafeError>>,
}

impl Default for DaemonControlState {
    fn default() -> Self {
        Self {
            selected: DaemonAction::Restart,
            pending: None,
            result: None,
        }
    }
}

impl DaemonControlState {
    #[must_use]
    pub const fn selected(&self) -> DaemonAction {
        self.selected
    }

    #[must_use]
    pub const fn pending(&self) -> Option<DaemonAction> {
        match self.pending {
            Some((action, _)) => Some(action),
            None => None,
        }
    }

    #[must_use]
    pub const fn result(&self) -> Option<&Result<Notice, SafeError>> {
        self.result.as_ref()
    }
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

/// Workspace-global drawer that currently owns root-pane input.
///
/// Director and the workspace terminal may both be visible. This focus is
/// therefore independent from either drawer's open flag and follows the most
/// recently opened surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDrawerFocus {
    Director,
    Terminal,
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
    /// Home's right-anchored Director mode drawer.
    director_drawer_open: bool,
    /// Explicit screen inside the Director shell. Closing the drawer preserves
    /// this route so reopening returns to the same stable Work Run context.
    director_route: DirectorRoute,
    /// Home's bottom-anchored workspace-root generic terminal drawer. It is
    /// independent from Director and preserves the managed Home state beneath
    /// both drawers.
    root_terminal_drawer_open: bool,
    /// Whether the open workspace terminal keeps the same drawer UI but uses
    /// the full available screen height.
    root_terminal_full_height: bool,
    /// The open drawer that owns root-pane input. `None` iff both drawers are
    /// closed; the reducer keeps this invariant through every transition.
    workspace_drawer_focus: Option<WorkspaceDrawerFocus>,
    /// Local `New` flow inside the Director mode drawer. Candidate
    /// availability is injected with [`AvailableModels`]; opening and moving
    /// this picker performs no daemon work.
    director_new: DirectorNew,
    /// Goal composer source. It is populated only in goal-driven mode and is
    /// moved into one daemon launch effect on confirmation.
    director_goal: String,
    /// One root launch submitted from the picker. It remains fenced until the
    /// shell reports the matching completion, so repeated Enter cannot mint a
    /// second operation while the first request is in flight.
    director_launching: Option<OperationId>,
    note_editor: Option<NoteEditor>,
    environment_editor: Option<EnvironmentEditor>,
    role_editor: Option<RoleEditor>,
    workflows: std::collections::BTreeMap<SessionId, super::workflow::WorkflowPanel>,
    daemon_control: DaemonControlState,
    decisions: Vec<UserDecision>,
    unread_decisions: std::collections::BTreeSet<UserDecisionId>,
    decision_overlay: Option<DecisionOverlayState>,
    pr_overlay: Option<PrOverlay>,
    cleanup_queue: Option<CleanupQueueState>,
    remove_queue: Option<RemoveQueueState>,
    preview_overlay: Option<PreviewOverlay>,
    create_session: Option<CreateSessionForm>,
    create_session_error: Option<Notice>,
    terminal_launch_error: Option<Notice>,
    agent_launch_error: Option<Notice>,
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    /// 表示中 session の name。新規作成の同名 validation にだけ使う advisory copy で、
    /// authoritative な identity は [`sessions`](Self::sessions) が持つ。
    session_names: Vec<String>,
    /// Per-session lifecycle by stable identity, used to gate actions by
    /// capability (attach only when `can_use`). A session absent here is treated
    /// as `Available`, so pre-lifecycle callers keep their behaviour.
    session_lifecycles: BTreeMap<SessionId, SessionLifecycleProjection>,
    /// Non-persistent daemon role assignment projection by stable identity.
    session_roles: BTreeMap<SessionId, SessionRoleProjection>,
    /// Daemon-authoritative PR rows by stable session identity. The sidebar and
    /// modal deliberately read this same projection.
    session_prs: BTreeMap<SessionId, (u64, Vec<PrEntry>)>,
    session_pr_revision: u64,
    pr_auto_open: PrAutoOpen,
    pr_merge_celebrations: BTreeMap<SessionId, u64>,
    /// Read-only effective session-scope catalog used by the create picker.
    role_catalog: SessionRoleCatalog,
    branch_catalog: SessionBranchCatalog,
    selected: Selection,
    /// Managed session shown in Closeup. `None` means Home has no managed
    /// session target; workspace-root scope is deliberately not a fallback.
    active: Option<SessionId>,
    notice: Option<Notice>,
    runtimes: Vec<RuntimePhase>,
    feedback: Option<Feedback>,
    pending: Vec<PendingOperation>,
    next_pending_token: u64,
    interaction_count: u64,
    mascot_tick: u64,
    size: Option<(u16, u16)>,
    /// Whether presentation can currently draw the Garden without hiding Home
    /// behind an invisible overlay. The renderer injects this layout fact; the
    /// reducer uses it to admit both automatic and manual opening consistently.
    garden_available: bool,
    garden_sidebar_scroll: usize,
    /// Last session press eligible to become the first half of a double click.
    /// The controller owns this stable identity after hit-testing; the shell
    /// supplies only coordinates and a monotonic timestamp.
    pending_session_click: Option<(SessionId, std::time::Duration)>,
    has_live_pane: bool,
    /// Whether the active target owns any pane tab — live, pending, ready, or
    /// interrupted history. The Closeup action modal is the launcher for an empty
    /// pane, so a target that owns a tab shows its tab strip instead (#510).
    has_pane_tab: bool,
    closeup_action_forced: bool,
    /// Agent CLIs installed on this machine, injected from the composition
    /// root's probe. Closeup refuses a `-m` selection outside this set instead
    /// of sending a launch the daemon cannot run.
    available_models: AvailableModels,
    /// The configured provider a Closeup `agent` without `-m` launches.
    default_model: DefaultModel,
    /// Compatibility defaults to the historical conversation picker.
    work_mode: WorkMode,
    ctrl_c_grace: bool,
    /// Focus of the exit prompt's three buttons. Opening the overlay resets it
    /// to [`ExitChoice::Quit`], so the historical `Ctrl-Q` + `Enter` still ends
    /// the process. The presentation layer projects this into the shared choice
    /// widget, keeping that widget out of this usecase-layer state.
    exit_choice: ExitChoice,
    /// Stable failed-delete target and focused answer (`true` = Yes).
    force_remove_confirmation: Option<(SessionId, bool)>,
}

/// The three answers the workspace exit prompt accepts.
///
/// Leaving and quitting are deliberately distinct: `Welcome` keeps the process
/// alive so another workspace can be opened without a restart, while `Quit` ends
/// the TUI. Neither is reachable without passing through the prompt, so a
/// mis-typed chord cannot fall into the wrong one (#556).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitChoice {
    /// Leave this workspace and return to the Welcome switcher. The process, and
    /// every daemon-owned terminal, keeps running.
    Welcome,
    /// End this TUI client. Daemon-owned terminals keep running.
    Quit,
    /// Stay in this workspace.
    Stay,
}

/// The drawer-local `New` flow.
///
/// `Empty` is deliberately distinct from `Choosing`: a machine with no
/// installed CLI never opens a zero-row picker and therefore cannot submit a
/// daemon request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DirectorNew {
    /// The conversation drawer is visible without its CLI chooser.
    #[default]
    Idle,
    /// An installed provider is highlighted but not yet confirmed.
    Choosing(DefaultModel),
    /// No supported Agent CLI is installed.
    Empty,
}

/// Parent restored by the Director-local back command from a Console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectorConsoleParent {
    Organization,
    RunOverview(SupervisorRunId),
}

/// Explicit Director screen hierarchy. Transient Start/confirmation states are
/// layered over one of these retained routes and return to it on cancellation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DirectorRoute {
    #[default]
    Organization,
    WorkRuns,
    RunOverview(SupervisorRunId),
    Console(DirectorConsoleParent),
}

impl DirectorRoute {
    /// Primary Director surface for one workspace interaction model.
    ///
    /// Classic work starts from the workspace organization, while goal-driven
    /// work starts from its durable Work Run inventory. Retained routes still
    /// win when the configured mode has not changed.
    #[must_use]
    const fn landing_for(mode: WorkMode) -> Self {
        match mode {
            WorkMode::Classic => Self::Organization,
            WorkMode::GoalDriven => Self::WorkRuns,
        }
    }

    /// Whether this retained route belongs to the selected workflow.
    ///
    /// Classic conversations and goal-driven Work Runs are separate screen
    /// trees. A workflow switch keeps daemon-owned work alive, but never
    /// exposes the previous workflow's route in the newly selected tree.
    #[must_use]
    const fn belongs_to(self, mode: WorkMode) -> bool {
        matches!(
            (mode, self),
            (
                WorkMode::Classic,
                Self::Organization | Self::Console(DirectorConsoleParent::Organization)
            ) | (
                WorkMode::GoalDriven,
                Self::WorkRuns
                    | Self::RunOverview(_)
                    | Self::Console(DirectorConsoleParent::RunOverview(_))
            )
        )
    }
}

impl ExitChoice {
    /// The prompt's buttons in display order. `Stay` is the cancel button and is
    /// therefore last, matching the `[ yes ] [ no ]` ordering of the two-choice
    /// confirmations.
    pub const ORDER: [Self; 3] = [Self::Welcome, Self::Quit, Self::Stay];

    /// Index of this choice in [`Self::ORDER`], for the button row's focus.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Welcome => 0,
            Self::Quit => 1,
            Self::Stay => 2,
        }
    }

    /// Move focus by one button, wrapping at both ends.
    #[must_use]
    const fn shifted(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Welcome, true) | (Self::Stay, false) => Self::Quit,
            (Self::Quit, true) | (Self::Welcome, false) => Self::Stay,
            (Self::Stay, true) | (Self::Quit, false) => Self::Welcome,
        }
    }

    /// The effects committing this choice asks the backend to run. Staying is the
    /// only answer with no effect.
    fn effects(self) -> Vec<Effect> {
        match self {
            Self::Welcome => vec![Effect::LeaveWorkspace],
            Self::Quit => vec![Effect::Detach],
            Self::Stay => Vec::new(),
        }
    }
}

impl AppState {
    /// The first managed session is selected/active when present. An empty Home
    /// starts neutral instead of implicitly selecting the new-session action.
    #[must_use]
    pub fn home(workspace: WorkspaceId, sessions: Vec<SessionId>) -> Self {
        let active = sessions.first().copied();
        let selected = active.map_or(Selection::Idle, |session| {
            Selection::Target(Target::Session(session))
        });
        Self {
            route: Route::Home(HomeMode::Switch),
            overlay: None,
            director_drawer_open: false,
            director_route: DirectorRoute::Organization,
            root_terminal_drawer_open: false,
            root_terminal_full_height: false,
            workspace_drawer_focus: None,
            director_new: DirectorNew::Idle,
            director_goal: String::new(),
            director_launching: None,
            note_editor: None,
            environment_editor: None,
            role_editor: None,
            workflows: std::collections::BTreeMap::new(),
            daemon_control: DaemonControlState::default(),
            decisions: Vec::new(),
            unread_decisions: std::collections::BTreeSet::new(),
            decision_overlay: None,
            pr_overlay: None,
            cleanup_queue: None,
            remove_queue: None,
            preview_overlay: None,
            create_session: None,
            create_session_error: None,
            terminal_launch_error: None,
            agent_launch_error: None,
            workspace,
            sessions,
            session_names: Vec::new(),
            session_lifecycles: BTreeMap::new(),
            session_roles: BTreeMap::new(),
            session_prs: BTreeMap::new(),
            session_pr_revision: 0,
            pr_auto_open: PrAutoOpen::default(),
            pr_merge_celebrations: BTreeMap::new(),
            role_catalog: SessionRoleCatalog::default(),
            branch_catalog: SessionBranchCatalog::default(),
            selected,
            active,
            notice: None,
            runtimes: Vec::new(),
            feedback: None,
            pending: Vec::new(),
            next_pending_token: 1,
            interaction_count: 0,
            mascot_tick: 0,
            size: None,
            garden_available: true,
            garden_sidebar_scroll: 0,
            pending_session_click: None,
            has_live_pane: false,
            has_pane_tab: false,
            closeup_action_forced: false,
            available_models: AvailableModels::all(),
            default_model: DefaultModel::default(),
            work_mode: WorkMode::default(),
            ctrl_c_grace: false,
            exit_choice: ExitChoice::Quit,
            force_remove_confirmation: None,
        }
    }

    /// 常駐 route。
    #[must_use]
    pub const fn route(&self) -> Route {
        self.route
    }
    /// Requested scroll offset in the Garden session list.
    #[must_use]
    pub const fn garden_sidebar_scroll(&self) -> usize {
        self.garden_sidebar_scroll
    }

    /// 最前面 overlay。閉じても [`route`](Self::route) は変わらない。
    #[must_use]
    pub const fn overlay(&self) -> Option<Overlay> {
        self.overlay
    }
    /// Whether the frontmost Director mode drawer owns Home input.
    #[must_use]
    pub const fn director_drawer_open(&self) -> bool {
        self.director_drawer_open
    }
    /// Screen currently retained inside Director.
    #[must_use]
    pub const fn director_route(&self) -> DirectorRoute {
        self.director_route
    }
    /// Whether the workspace-root terminal drawer owns Home input.
    #[must_use]
    pub const fn root_terminal_drawer_open(&self) -> bool {
        self.root_terminal_drawer_open
    }
    /// Whether the workspace terminal occupies the full available height.
    #[must_use]
    pub const fn root_terminal_full_height(&self) -> bool {
        self.root_terminal_full_height
    }
    /// Whether either workspace-global drawer is frontmost.
    #[must_use]
    pub const fn workspace_drawer_open(&self) -> bool {
        self.director_drawer_open || self.root_terminal_drawer_open
    }
    /// The visible workspace-global drawer that currently owns input.
    #[must_use]
    pub const fn workspace_drawer_focus(&self) -> Option<WorkspaceDrawerFocus> {
        self.workspace_drawer_focus
    }
    /// Current local state of the drawer's explicit Agent CLI chooser.
    #[must_use]
    pub const fn director_new(&self) -> DirectorNew {
        self.director_new
    }
    /// Operation currently fenced by the drawer's root launch flow.
    #[must_use]
    pub const fn director_launching(&self) -> Option<OperationId> {
        self.director_launching
    }
    /// Home sidebar mascot animation frame. Only [`AppEvent::Tick`] advances it.
    #[must_use]
    pub const fn mascot_tick(&self) -> u64 {
        self.mascot_tick
    }
    /// Open note editor, including unsaved values after a save failure.
    #[must_use]
    pub fn note_editor(&self) -> Option<&NoteEditor> {
        self.note_editor.as_ref()
    }
    /// Open new-session form, including values retained after validation failure.
    #[must_use]
    pub fn create_session_form(&self) -> Option<&CreateSessionForm> {
        self.create_session.as_ref()
    }
    /// Safe message for the create-failure dialog, present exactly while
    /// [`Overlay::CreateSessionError`] is open.
    #[must_use]
    pub fn create_session_error(&self) -> Option<&Notice> {
        self.create_session_error.as_ref()
    }
    /// Safe message for the terminal-launch failure dialog, present exactly
    /// while [`Overlay::TerminalLaunchError`] is open.
    #[must_use]
    pub fn terminal_launch_error(&self) -> Option<&Notice> {
        self.terminal_launch_error.as_ref()
    }
    /// Safe message for the Agent-launch failure dialog, present exactly while
    /// [`Overlay::AgentLaunchError`] is open.
    #[must_use]
    pub fn agent_launch_error(&self) -> Option<&Notice> {
        self.agent_launch_error.as_ref()
    }
    /// Open environment editor, including unsaved values after a save failure.
    #[must_use]
    pub fn environment_editor(&self) -> Option<&EnvironmentEditor> {
        self.environment_editor.as_ref()
    }
    #[must_use]
    pub fn role_editor(&self) -> Option<&RoleEditor> {
        self.role_editor.as_ref()
    }

    #[must_use]
    pub fn workflow_panel(&self, session: SessionId) -> Option<&super::workflow::WorkflowPanel> {
        self.workflows.get(&session)
    }
    /// Current selection, pending action, and safe result in the daemon modal.
    #[must_use]
    pub const fn daemon_control(&self) -> &DaemonControlState {
        &self.daemon_control
    }
    /// Pending decisions from the current workspace only.
    #[must_use]
    pub fn decisions(&self) -> &[UserDecision] {
        &self.decisions
    }
    /// Pending decisions the user has not opened from the notice centre yet.
    #[must_use]
    pub fn unread_decision_ids(&self) -> &std::collections::BTreeSet<UserDecisionId> {
        &self.unread_decisions
    }
    /// Open decision list/editor state, if its overlay is visible.
    #[must_use]
    pub fn decision_overlay(&self) -> Option<&DecisionOverlayState> {
        self.decision_overlay.as_ref()
    }
    /// Open Pull Request overlay state, including its cursor and any safe error.
    #[must_use]
    pub fn pr_overlay(&self) -> Option<&PrOverlay> {
        self.pr_overlay.as_ref()
    }
    /// Open merge-confirmed cleanup queue, including its stable selection.
    #[must_use]
    pub fn cleanup_queue(&self) -> Option<&CleanupQueueState> {
        self.cleanup_queue.as_ref()
    }
    /// Open explicit session-removal checklist, including stable selection.
    #[must_use]
    pub fn remove_queue(&self) -> Option<&RemoveQueueState> {
        self.remove_queue.as_ref()
    }
    /// Open file preview overlay state, including finder and document state.
    #[must_use]
    pub fn preview_overlay(&self) -> Option<&PreviewOverlay> {
        self.preview_overlay.as_ref()
    }
    /// navigation cursor。
    #[must_use]
    pub const fn selected(&self) -> Selection {
        self.selected
    }
    /// command / Closeup の managed session。
    #[must_use]
    pub const fn active(&self) -> Option<SessionId> {
        self.active
    }
    /// この Home が投影している workspace identity。
    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }
    /// snapshot の stable session identity。
    #[must_use]
    pub fn sessions(&self) -> &[SessionId] {
        &self.sessions
    }
    /// 表示中 session の name（同名 validation 用の advisory copy）。
    #[must_use]
    pub fn session_names(&self) -> &[String] {
        &self.session_names
    }
    /// Per-session lifecycle by stable identity, for the runtime sync guard.
    #[must_use]
    pub fn session_lifecycles(&self) -> &BTreeMap<SessionId, SessionLifecycleProjection> {
        &self.session_lifecycles
    }
    #[must_use]
    pub fn session_roles(&self) -> &BTreeMap<SessionId, SessionRoleProjection> {
        &self.session_roles
    }
    /// Latest daemon PR rows for one stable session identity.
    #[must_use]
    pub fn session_prs(&self, session: SessionId) -> Option<&[PrEntry]> {
        self.session_prs
            .get(&session)
            .map(|(_, prs)| prs.as_slice())
    }
    /// Local generation for change-driven sidebar row reconstruction.
    #[must_use]
    pub const fn session_pr_revision(&self) -> u64 {
        self.session_pr_revision
    }
    pub fn set_pr_auto_open(&mut self, mode: PrAutoOpen) {
        self.pr_auto_open = mode;
    }
    #[must_use]
    pub fn celebrates_pr_merge(&self, session: SessionId) -> bool {
        self.pr_merge_celebrations
            .get(&session)
            .is_some_and(|until| self.mascot_tick <= *until)
    }
    #[must_use]
    pub fn role_catalog(&self) -> &SessionRoleCatalog {
        &self.role_catalog
    }
    /// Whether the session at this stable identity is a usable (attachable)
    /// checkout. A session with no lifecycle projection is treated as usable;
    /// a `Failed` row reports `false`, so attach is not offered.
    #[must_use]
    fn session_can_use(&self, session: SessionId) -> bool {
        self.session_lifecycles
            .get(&session)
            .is_none_or(|lifecycle| lifecycle.capabilities().can_use)
    }
    /// Whether the session at this stable identity accepts a removal request.
    /// Legacy callers without lifecycle projections keep their established
    /// behavior, while a daemon-authoritative `Deleting` row cannot be retried.
    #[must_use]
    fn session_can_remove(&self, session: SessionId) -> bool {
        self.session_lifecycles
            .get(&session)
            .is_none_or(|lifecycle| lifecycle.capabilities().can_remove)
    }
    /// A cleanup candidate has a fully observed, merge-only visible PR set and
    /// no Agent which may still be producing work. The daemon remains the final
    /// authority for dirty worktrees, terminals, and Git teardown.
    fn session_can_cleanup(&self, session: SessionId) -> bool {
        let Some(prs) = self.session_prs(session) else {
            return false;
        };
        let mut visible = prs.iter().filter(|pr| pr.state != PrState::Dismissed);
        let Some(first) = visible.next() else {
            return false;
        };
        first.state == PrState::Merged
            && visible.all(|pr| pr.state == PrState::Merged)
            && self.session_can_remove(session)
            && matches!(
                self.phase_for(Target::Session(session)),
                TargetPhase::Absent | TargetPhase::Done
            )
    }
    /// 最後の safe notice。
    #[must_use]
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }
    /// 実行中操作。
    #[must_use]
    pub fn pending(&self) -> &[PendingOperation] {
        &self.pending
    }
    /// Runtime phases retained for the current workspace only.
    #[must_use]
    pub fn runtimes(&self) -> &[RuntimePhase] {
        &self.runtimes
    }
    /// The current safe feedback for the fixed Home feedback area.
    #[must_use]
    pub fn feedback(&self) -> Option<&Feedback> {
        self.feedback.as_ref()
    }
    /// Aggregates phase for a target using `done > waiting > running > ready > absent`.
    #[must_use]
    pub fn phase_for(&self, target: Target) -> TargetPhase {
        let scope = target.session_id();
        self.runtimes
            .iter()
            .filter(|entry| entry.runtime.session_id == scope)
            .map(|entry| TargetPhase::from_agent_phase(entry.phase))
            .max_by_key(|phase| phase.rank())
            .unwrap_or(TargetPhase::Absent)
    }
    /// 最後に受け取った terminal geometry。
    #[must_use]
    pub const fn size(&self) -> Option<(u16, u16)> {
        self.size
    }
    /// Whether the current Home projection has a live terminal or Agent pane.
    #[must_use]
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
    pub const fn interaction_count(&self) -> u64 {
        self.interaction_count
    }
    /// Whether the next management `Ctrl-C` is deliberately absorbed.
    #[must_use]
    pub const fn ctrl_c_grace(&self) -> bool {
        self.ctrl_c_grace
    }
    /// Which button the exit prompt currently focuses. The presentation layer
    /// reads this to draw the shared choice buttons in the right state.
    #[must_use]
    pub const fn exit_choice(&self) -> ExitChoice {
        self.exit_choice
    }
    /// Failed-delete target and whether Yes is focused while its prompt is open.
    #[must_use]
    pub const fn force_remove_confirmation(&self) -> Option<(SessionId, bool)> {
        self.force_remove_confirmation
    }
    /// The Agent CLIs a Closeup `agent` command may select.
    #[must_use]
    pub const fn available_models(&self) -> AvailableModels {
        self.available_models
    }
    /// The provider a Closeup `agent` without `-m` launches.
    #[must_use]
    pub const fn default_model(&self) -> DefaultModel {
        self.default_model
    }
    /// Configured Director interaction model.
    #[must_use]
    pub const fn work_mode(&self) -> WorkMode {
        self.work_mode
    }
    /// Current goal composer text. Empty in classic mode and after admission.
    #[must_use]
    pub fn director_goal(&self) -> &str {
        &self.director_goal
    }
    /// Apply the observed CLI availability and the configured default provider.
    /// The composition root supplies both, so this usecase performs no PATH or
    /// settings IO of its own.
    pub const fn set_agent_models(&mut self, available: AvailableModels, default: DefaultModel) {
        self.available_models = available;
        self.default_model = default;
    }

    /// Apply the effective workspace interaction setting.
    ///
    /// A real mode transition selects that workflow's primary Director
    /// surface. Re-applying the same effective setting preserves the retained
    /// route used when the drawer is closed and reopened. Returning to classic
    /// also drops a draft that was never submitted, but never touches a live
    /// Agent or Work Run.
    pub fn set_work_mode(&mut self, mode: WorkMode) {
        if self.work_mode == mode {
            return;
        }
        self.work_mode = mode;
        self.director_route = DirectorRoute::landing_for(mode);
        if mode == WorkMode::Classic {
            self.director_goal.clear();
        }
    }

    /// Convert the managed active session to the target vocabulary used at
    /// daemon/pane boundaries. This never manufactures `Target::Root`.
    fn active_target(&self) -> Option<Target> {
        self.active.map(Target::Session)
    }

    /// Resolve the session whose files the Preview overlay may search.
    ///
    /// Switch is a cursor-driven inspection surface, so Preview follows the
    /// selected sidebar session just like the right-pane preview. Closeup keeps
    /// operating on its active session. Neither route manufactures a workspace
    /// root target or accepts a stale session identity.
    fn file_preview_target(&self) -> Option<Target> {
        let session = match self.route {
            Route::Home(HomeMode::Switch) => match self.selected {
                Selection::Target(Target::Session(session)) => session,
                Selection::Idle | Selection::Target(Target::Root(_)) | Selection::NewSession => {
                    return None;
                }
            },
            Route::Home(HomeMode::Closeup) => self.active?,
        };
        self.sessions
            .contains(&session)
            .then_some(Target::Session(session))
    }

    fn rows(&self) -> Vec<Selection> {
        let mut rows = Vec::with_capacity(self.sessions.len() + 1);
        rows.extend(
            self.sessions
                .iter()
                .copied()
                .map(|id| Selection::Target(Target::Session(id))),
        );
        rows.push(Selection::NewSession);
        rows
    }

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
    /// header, the mascot sidecar, the footer, or a click
    /// outside the sidebar body.
    ///
    /// This is the controller-owned hit-test the pointer reducer shares with the
    /// `home_left_pane` render: it mirrors the chrome rows, the left/right split,
    /// the foot-of-sidebar mascot reservation, and the scroll offset so a click
    /// always lands on the row the user sees. It reads the last terminal geometry
    /// from [`AppState::size`], so a pointer event before the first resize is
    /// inert.
    fn sidebar_selection_at(&self, column: u16, row: u16) -> Option<Selection> {
        self.sidebar_hit_at(column, row).map(|hit| hit.selection)
    }

    fn sidebar_hit_at(&self, column: u16, row: u16) -> Option<SidebarHit> {
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
            return (usize::from(row) == SIDEBAR_CHROME_ROWS).then(|| SidebarHit {
                selection: rows[0],
                line: 0,
                left,
            });
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
                return Some(SidebarHit {
                    selection: *entry,
                    line: clicked - offset,
                    left,
                });
            }
            offset += lines;
        }
        None
    }

    /// Resolve only the visible `<PR icon> <count>` cells at the right edge of
    /// a session metadata row. The count and width mirror the view's badge.
    fn sidebar_pr_at(&self, column: u16, row: u16) -> Option<SessionId> {
        let hit = self.sidebar_hit_at(column, row)?;
        let Selection::Target(Target::Session(session)) = hit.selection else {
            return None;
        };
        if hit.line != 1 {
            return None;
        }
        let visible = self
            .session_prs(session)?
            .iter()
            .filter(|pr| pr.is_visible())
            .count();
        if visible == 0 {
            return None;
        }
        let badge_width = 2 + visible.to_string().len();
        let start = hit.left.saturating_sub(badge_width);
        (start..hit.left)
            .contains(&usize::from(column))
            .then_some(session)
    }

    fn reconcile_sessions(&mut self, previous_sessions: &[SessionId]) {
        let rows = self.rows();
        if !rows.contains(&self.selected) {
            self.selected = match self.selected {
                Selection::Target(Target::Session(session)) => {
                    replacement_session(previous_sessions, &self.sessions, session, |_| true)
                        .map_or(Selection::Idle, |session| {
                            Selection::Target(Target::Session(session))
                        })
                }
                Selection::Idle | Selection::Target(Target::Root(_)) | Selection::NewSession => {
                    self.sessions
                        .first()
                        .copied()
                        .map_or(Selection::Idle, |session| {
                            Selection::Target(Target::Session(session))
                        })
                }
            };
        }
        if let Some(active) = self.active
            && (!self.sessions.contains(&active) || !self.session_can_use(active))
        {
            self.active =
                replacement_session(previous_sessions, &self.sessions, active, |session| {
                    self.session_can_use(session)
                });
        }
        if self.active.is_none() {
            self.route = Route::Home(HomeMode::Switch);
            if self.overlay == Some(Overlay::Closeup) {
                self.overlay = None;
                self.closeup_action_forced = false;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SidebarHit {
    selection: Selection,
    /// Zero-based line inside the logical sidebar row.
    line: usize,
    /// Width of the rendered left pane.
    left: usize,
}

/// Choose a deterministic surviving row at the removed session's former
/// display position. Candidates after that position win; otherwise the last
/// earlier candidate wins. The predicate lets active reconciliation exclude
/// failed/deleting sessions while cursor reconciliation retains their rows.
fn replacement_session(
    previous: &[SessionId],
    current: &[SessionId],
    removed: SessionId,
    eligible: impl Fn(SessionId) -> bool,
) -> Option<SessionId> {
    let former_index = previous
        .iter()
        .position(|session| *session == removed)
        .unwrap_or_default();
    current
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, session)| eligible(*session))
        .find(|(index, _)| *index >= former_index)
        .map(|(_, session)| session)
        .or_else(|| current.iter().copied().rfind(|session| eligible(*session)))
}

/// Fixed chrome rows (title + spacer) above the Home sidebar's row body. Mirrors
/// `views::workspace`'s `CHROME_ROWS`; a compile-time assertion there keeps the
/// hit-test and the render agreeing on the geometry.
pub(crate) const SIDEBAR_CHROME_ROWS: usize = 2;
/// Desired Home sidebar left-pane width. The split clamps it to leave the right
/// pane at least one column. Mirrors `views::workspace`'s `LEFT_WIDTH`.
pub(crate) const SIDEBAR_LEFT_WIDTH: usize = 36;
/// Lines one session row occupies: its summary, its change history, and its
/// Agent states. The count never varies with how many Agents a session has —
/// this hit-test only knows runtime-local phases, while the view also folds in
/// the daemon Agent inventory, so an Agent-dependent height would drift by a
/// row and land clicks on the wrong session. Mirrors `views::workspace`'s
/// `SESSION_ROW_LINES`, which asserts the two agree at compile time.
pub(crate) const SIDEBAR_SESSION_ROW_LINES: usize = 3;
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

/// Scroll rows the sidebar viewport uses to weight one row: a session spans its
/// summary, change-history, and Agent lines, and the `+ new session` action is a
/// single line.
fn sidebar_row_height(row: Selection) -> usize {
    match row {
        Selection::Target(Target::Session(_)) => SIDEBAR_SESSION_ROW_LINES,
        // Root is not a Home row. Treat a stale/private synthetic value as one
        // line so geometry remains total without reintroducing a divider.
        Selection::Idle | Selection::Target(Target::Root(_)) | Selection::NewSession => 1,
    }
}

/// Body lines a row actually draws: a session identity row carries a metadata
/// row and an Agent row, while the action row is a single line.
fn sidebar_row_content_lines(row: Selection) -> usize {
    match row {
        Selection::Target(Target::Session(_)) => SIDEBAR_SESSION_ROW_LINES,
        Selection::Idle | Selection::Target(Target::Root(_)) | Selection::NewSession => 1,
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
    /// Select the previous usable managed session. Closeup also activates it.
    PreviousSession,
    /// Select the next usable managed session. Closeup also activates it.
    NextSession,
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
    /// Insert one bracketed-paste payload into the focused Home input.
    Paste(String),
    /// Ctrl-A / terminals that decode it as Home while Home is in Switch mode.
    CtrlA,
    /// Ctrl-O returns Closeup to Switch. It has no effect while already in Switch.
    CtrlO,
    /// Ctrl-O ] selects the next Closeup tab.
    CtrlN,
    /// Ctrl-O [ selects the previous Closeup tab.
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
    /// Remove the selected session, purging it when it is a diagnosed integrity
    /// orphan, or dismiss the selected object from another management surface.
    CtrlX,
    /// Open the detach confirmation from a reserved live-pane action.
    OpenQuitConfirmation,
    /// Toggle the frontmost Director mode drawer. Opening is ignored while an
    /// existing modal overlay owns input; while open, this and Escape are the
    /// only keys that mutate Home state.
    ToggleDirectorDrawer,
    /// Toggle the bottom workspace-root terminal drawer (`Ctrl-O Ctrl-T`, with
    /// `Ctrl-O t` retained for compatibility). Opening explicitly asks the
    /// daemon to open or reuse the trusted root shell.
    ToggleRootTerminalDrawer,
    /// Toggle the open workspace terminal between drawer and full-height modes.
    ToggleRootTerminalFullHeight,
    /// Open a new tab in the already-open workspace-root terminal drawer.
    OpenRootTerminal,
    /// Open the Director mode drawer and its explicit New CLI picker.
    OpenDirectorNew,
    /// Open the classic Director Organization screen.
    OpenDirectorOrganization,
    /// Open the Work Run list directly.
    OpenDirectorWorkRuns,
    /// Open one stable Work Run observation.
    OpenDirectorRunOverview(SupervisorRunId),
    /// Open the selected root Director Agent from its retained parent.
    OpenDirectorConsole(DirectorConsoleParent),
    /// Move one level up inside Director without closing the drawer.
    DirectorBack,
    /// workspace scope overlay を開く。
    OpenOverview,
    /// workspace Garden を直接開く。
    OpenGarden,
    /// target scope overlay を開く。
    OpenCloseupOverlay,
    /// Open the active target's scratchpad. No keyboard chord is assigned here.
    OpenNotes,
    /// Open the active target's environment editor.
    OpenEnvironment,
    /// Open the active target's Pull Request list overlay (`Ctrl-O p`).
    OpenPrs,
    /// Open the selected session's file preview overlay.
    OpenPreview,
    /// Open the current workspace's durable pending decision list.
    OpenDecisions,
    /// Move within the pending list or current decision options.
    DecisionPrevious,
    /// Move within the pending list or current decision options.
    DecisionNext,
    /// Scroll the active viewport one page towards its beginning.
    PageUp,
    /// Scroll the active viewport one page towards its end.
    PageDown,
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
    /// Persist the current environment through its owning port.
    SaveEnvironment,
    /// Toggle and save the raw, lossless role catalog editor.
    ToggleRoleScope,
    SaveRoles,
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
pub fn classify_management_input(input: LiveInput) -> Option<AppKey> {
    let LiveInput::Key(key) = input else {
        return None;
    };
    if key.kind == KeyEventKind::Release {
        return None;
    }
    match key.code {
        KeyCode::Char('x' | 'X') if is_control_and_shift(key.modifiers) => Some(AppKey::CtrlX),
        KeyCode::Char('s')
            if key.modifiers.control && !key.modifiers.shift && !key.modifiers.alt =>
        {
            Some(AppKey::SaveRoles)
        }
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
        KeyCode::Char('\u{18}') if !key.modifiers.shift && !key.modifiers.alt => {
            Some(AppKey::CtrlX)
        }
        KeyCode::Char('x')
            if key.modifiers.control && !key.modifiers.shift && !key.modifiers.alt =>
        {
            Some(AppKey::CtrlX)
        }
        KeyCode::Home => Some(AppKey::CtrlA),
        KeyCode::Enter => Some(AppKey::Enter),
        KeyCode::Tab => Some(AppKey::Tab),
        KeyCode::Backspace => Some(AppKey::Backspace),
        KeyCode::Escape => Some(AppKey::Escape),
        KeyCode::PageUp => Some(AppKey::PageUp),
        KeyCode::PageDown => Some(AppKey::PageDown),
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
    WorkflowEdit {
        session: SessionId,
        edit: super::workflow::WorkflowEdit,
    },
    /// Input captured by the selected native workflow pane, never by a PTY.
    WorkflowInput { session: SessionId, key: AppKey },
    /// live terminal input。現行 Home reducer は接続 seam を提供し、pane routing は runtime 合成側が担う。
    Input(LiveInput),
    /// The runtime's current live-pane availability, sampled on every event.
    /// The reducer treats it as a *level* and reacts only on the edge: a
    /// live→non-live transition arms the one-shot Ctrl-C grace, while a
    /// non-live→live transition drops an unforced Closeup action modal. A
    /// re-sampled level that has not changed is inert, so an overlay opened in
    /// the same event batch (quit confirmation, PR / Preview, notes) and the
    /// Ctrl-C grace both survive the next sample.
    LivePaneAvailability(bool),
    /// Whether the active target owns any pane tab, sampled by the shell from
    /// the pane registry.
    ///
    /// Like [`Self::LivePaneAvailability`] this is a level, and only its edge
    /// matters. A non-live tab — an interrupted Agent history above all — can be
    /// selected and acted on without changing overlay state. A forced action
    /// modal and every other overlay are left untouched.
    ///
    /// `error` is the pane's safe failure message (set when a launch failed
    /// rather than the pane exiting cleanly). When the last tab disappears, a
    /// `Some` value becomes the empty Closeup's notice so the user sees why the
    /// pane never came up instead of a silent bounce back.
    PaneTabAvailability {
        available: bool,
        error: Option<String>,
    },
    /// Open the selected unusable session only to reach pane tabs the daemon
    /// still owns. The runtime emits this event after proving that exact target
    /// has a retained tab, so a failed/deleting checkout gains no launch or
    /// filesystem capability while its Agent can still receive Ctrl-D.
    RetainedPaneActivated(Target),
    /// キー入力。
    Key(AppKey),
    /// terminal size の変更。
    Resize { width: u16, height: u16 },
    /// Presentation-level Garden layout availability for the current terminal.
    GardenAvailability(bool),
    /// 定期 tick。
    Tick,
    /// backend snapshot / notice。
    Backend(BackendEvent),
    /// request completion。
    OperationResult(OperationResult),
    /// The shell finished exactly one drawer-originated workspace-root Agent
    /// launch. A mismatched operation is ignored, preserving the in-flight
    /// fence against stale or replayed completions.
    DirectorLaunchFinished {
        operation: OperationId,
        supervisor_run_id: Option<SupervisorRunId>,
        succeeded: bool,
    },
    /// One terminal open request failed after it left the reducer. The message
    /// is presentation-safe and becomes a dismissible dialog when no other
    /// modal owns input; otherwise it remains available through the Home notice.
    TerminalLaunchFailed(Notice),
    /// One Agent launch request failed after it left the reducer. The message
    /// is presentation-safe and becomes a dismissible dialog when no other
    /// modal owns input; otherwise it remains available through the Home notice.
    AgentLaunchFailed(Notice),
    /// The runtime observed that the workspace-root Shell drawer owns no tabs.
    /// This is an explicit close rather than the user-facing toggle: both
    /// workspace drawers may be open while Director owns focus, and replaying a
    /// toggle in that state would open/focus Shell and request a new terminal.
    RootTerminalDrawerEmptied,
    /// The runtime observed the last active workspace-root Agent disappearing.
    /// Interrupted history remains available when Director is opened again,
    /// but the foreground drawer must not linger without an interactive Agent.
    DirectorDrawerEmptied,
    /// A pointer press moved input ownership to one of the already-open
    /// workspace drawers. Geometry and z-order stay presentation concerns; the
    /// reducer owns the focus invariant shared with keyboard toggles.
    WorkspaceDrawerFocused(WorkspaceDrawerFocus),
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
    /// Time elapsed since the last *user* interaction, measured by the shell on a
    /// monotonic clock. The reducer holds no clock of its own (design
    /// `document/proposals/15-session-garden.md`): it only compares the injected
    /// duration against [`GARDEN_IDLE_THRESHOLD`] and opens the screen saver when
    /// the Home underneath is eligible.
    ///
    /// Ticks, daemon/backend events, and Agent output are not interactions and
    /// never reach this event, so a workspace whose Agents are busy still shows
    /// the garden once its *user* has stopped touching the keyboard.
    IdleElapsed(std::time::Duration),
    /// An interaction with the open Garden, already resolved against the
    /// frame's own plot layout by presentation. The reducer never sees a cell or
    /// terminal capacity, so CJK labels and resize cannot
    /// move a rabbit away from the session it draws.
    GardenClick(GardenClick),
    /// Focus one stable session row without activating its Closeup. The process
    /// deck uses this when returning to a workspace whose controller was torn
    /// down during a project switch.
    FocusSession(SessionId),
    /// Open one stable session without relying on list position. The process
    /// deck uses this after a Garden visit switched to another workspace.
    VisitSession(SessionId),
    /// Presentation could not allocate the Garden's minimum layout. Manual
    /// commands fail visibly instead of leaving an invisible input owner.
    GardenUnavailable,
}

/// What an interaction with the open Garden resolved to.
///
/// Resolved by presentation from the `SessionId`- and `AgentRuntimeId`-tagged
/// rectangles the garden renderer returns for the frame currently on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GardenClick {
    /// Scroll only the session list, keeping Garden and the covered Home intact.
    Scroll { offset: usize },
    /// A session's plot. Its stable project/session pair becomes the process
    /// shell's visit target; this reducer activates it only when `workspace`
    /// names its own Home.
    ///
    /// `agent` is the exact rabbit that was pressed, when the press landed on
    /// one. The reducer's activation does not depend on it: the shell uses it to
    /// focus that Agent's own tab inside the Closeup this activation opens, and
    /// a rabbit whose tab has meanwhile gone simply lands on the session.
    Visit {
        workspace: WorkspaceId,
        session: SessionId,
        agent: Option<AgentRuntimeId>,
    },
    /// Anywhere else in the garden. The click is consumed and the Home from
    /// before the screen saver comes back.
    Dismiss,
}

/// How long Home must go without a user interaction before the Garden opens.
pub const GARDEN_IDLE_THRESHOLD: std::time::Duration = std::time::Duration::from_mins(5);

impl From<RuntimeEvent<BackendEvent>> for AppEvent {
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
    Workflow {
        job: super::workflow::WorkflowJob,
        result: Result<
            Box<usagi_core::domain::workflow::WorkflowSnapshot>,
            super::workflow::WorkflowError,
        >,
    },
    /// stable identity で表した session snapshot。
    Sessions(Vec<SessionId>),
    /// 表示中 session の name。新規作成の同名 validation にだけ使う advisory copy で、
    /// [`Sessions`](Self::Sessions) の identity 同期とは独立に還流してよい。
    SessionNames(Vec<String>),
    /// Per-session lifecycle keyed by stable identity, so the reducer can gate
    /// actions by capability (a `Failed` row is not attachable but is removable).
    /// A session absent here is treated as `Available`, so it refluxes
    /// independently of [`Sessions`](Self::Sessions).
    SessionLifecycles(BTreeMap<SessionId, SessionLifecycleProjection>),
    /// Safe daemon role metadata, independent of persisted `SessionRecord` rows.
    SessionRoles(BTreeMap<SessionId, SessionRoleProjection>),
    /// Effective session-scope picker catalog. Role instructions are omitted.
    SessionRoleCatalog(SessionRoleCatalog),
    /// Local and remote-tracking refs available as session bases.
    SessionBranchCatalog(SessionBranchCatalog),
    /// backend が safe と保証した notice。
    Notice(Notice),
    /// Completion of one daemon lifecycle action from the management modal.
    DaemonControlFinished {
        workspace: WorkspaceId,
        action: DaemonAction,
        token: PendingToken,
        result: Result<Notice, SafeError>,
    },
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
    /// Environment bindings returned by the settings owner: the edited scope's
    /// own bindings, plus the global ones a workspace inherits (empty when the
    /// edited scope *is* global).
    EnvironmentLoaded {
        scope: EnvScope,
        entries: Vec<EnvironmentEntry>,
        inherited: Vec<EnvironmentEntry>,
    },
    EnvironmentSaved {
        scope: EnvScope,
        entries: Vec<EnvironmentEntry>,
        inherited: Vec<EnvironmentEntry>,
    },
    /// A safe environment read/save failure.
    EnvironmentError { scope: EnvScope, error: SafeError },
    RolesLoaded {
        scope: RoleEditorScope,
        source: String,
    },
    RolesError {
        scope: RoleEditorScope,
        error: SafeError,
    },
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
    PullRequestsLoaded {
        target: Target,
        /// Monotonic daemon inventory revision. Late completions cannot replace
        /// a newer sidebar/modal projection.
        revision: u64,
        prs: Vec<PrEntry>,
    },
    /// A safe Pull Request read failure.
    PullRequestsError { target: Target, error: SafeError },
    /// Repository file candidates or one selected file returned by the overlay
    /// data owner. `path: None` carries the finder candidates; `Some` carries
    /// that file's safe UTF-8 lines.
    PreviewLoaded {
        target: Target,
        /// Unique identity of the request; value-equal A-B-A loads stay fenced.
        request_id: RequestId,
        path: Option<String>,
        /// Finder group that originated this request.
        filter: PreviewFileFilter,
        files: Vec<String>,
        lines: Vec<String>,
    },
    /// A safe preview read failure.
    PreviewError {
        target: Target,
        /// Unique identity of the request; value-equal A-B-A loads stay fenced.
        request_id: RequestId,
        path: Option<String>,
        /// Finder group that originated this request.
        filter: PreviewFileFilter,
        error: SafeError,
    },
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
    Workflow(super::workflow::WorkflowJob),
    /// Select the session's non-terminal workflow tab without launching an Agent.
    OpenWorkflow {
        session: SessionId,
    },
    /// Ask the pane owner to move its stable tab selection without exposing tab
    /// identities to this controller.
    SelectTab {
        direction: TabDirection,
    },
    /// session create を backend に依頼する。
    CreateSession {
        workspace: WorkspaceId,
        token: PendingToken,
        operation_id: OperationId,
        intent: SessionCreateIntent,
    },
    /// 次の snapshot を要求する。
    RefreshSessions {
        workspace: WorkspaceId,
    },
    /// workspace scope command を backend adapter に依頼する。
    WorkspaceCommand {
        workspace: WorkspaceId,
        command: overview::Command,
    },
    /// Execute one non-force daemon lifecycle action outside the daemon-owned
    /// connection being controlled.
    DaemonControl {
        workspace: WorkspaceId,
        action: DaemonAction,
        token: PendingToken,
    },
    /// Read an active target's scratchpad through the existing persistence owner.
    LoadNotes {
        target: Target,
    },
    /// Save an edited scratchpad through the existing persistence owner.
    SaveNotes {
        target: Target,
        scratchpad: Scratchpad,
    },
    /// Read one scope's environment bindings through the settings owner.
    LoadEnvironment {
        scope: EnvScope,
    },
    /// Save one scope's environment bindings through the settings owner.
    SaveEnvironment {
        scope: EnvScope,
        entries: Vec<EnvironmentEntry>,
    },
    LoadRoles {
        scope: RoleEditorScope,
    },
    SaveRoles {
        scope: RoleEditorScope,
        source: String,
    },
    /// Fetch the daemon-authoritative pending snapshot for one workspace.
    RefreshDecisions {
        workspace: WorkspaceId,
    },
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
    /// Open the selected target's worktree in the platform terminal. Unlike
    /// [`Self::OpenTerminal`], this does not create an embedded daemon pane.
    OpenExternalTerminal {
        target: Target,
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
    /// Start a workspace-root Director with one bounded goal and the daemon's
    /// autonomous delivery contract. Classic Agent launch remains a separate
    /// effect and request path.
    LaunchGoal {
        workspace: WorkspaceId,
        operation_id: OperationId,
        profile: Option<AgentProfileId>,
        goal: String,
    },
    /// Explicit provider-native resume for an interrupted session. The daemon
    /// validates retained metadata and creates a new PTY/runtime.
    ResumeAgent {
        workspace: WorkspaceId,
        session: SessionId,
        operation_id: OperationId,
    },
    /// Stop quiescent resumable Agents without removing their session.
    SleepSession {
        workspace: WorkspaceId,
        session: SessionId,
    },
    /// Clear one local continuation-scoped dismissal. This effect never asks
    /// the daemon to spawn or provider-resume a runtime.
    ReopenAgent {
        workspace: WorkspaceId,
        continuation: AgentContinuationRef,
    },
    /// selected session を削除する。root はこの effect に変換しない。
    RemoveSession {
        workspace: WorkspaceId,
        session: SessionId,
        force: bool,
        /// Whether the confirmed recovery may discard an unmerged session branch.
        force_delete_branch: bool,
        /// Whether the exact target is a diagnosed integrity orphan whose
        /// unregistered files and unmerged commits may be discarded.
        purge_orphan: bool,
    },
    /// Open a workspace and request the Home snapshot for this exact incarnation.
    ///
    /// The identity is deliberately not a name or path: a delayed completion for
    /// a different workspace must never replace the Home currently being opened.
    AttachWorkspace {
        workspace: WorkspaceId,
    },
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
    /// Detach this TUI client and end the process. The adapter owns the
    /// connection cleanup; this effect intentionally carries no terminal or
    /// operation cancellation.
    Detach,
    /// Leave this workspace for the Welcome switcher without ending the process.
    ///
    /// The connection cleanup is identical to [`Self::Detach`] — the adapter
    /// drops this workspace's ports, so the daemon releases the subscriptions and
    /// its terminals keep running. What differs is only what the shell does next:
    /// it re-enters the entry screens instead of returning from the TUI (#556).
    LeaveWorkspace,
    /// Read a target's Pull Request list through the daemon snapshot owner.
    /// The completion returns as [`BackendEvent::PullRequestsLoaded`] / `Error`.
    LoadPullRequests {
        target: Target,
    },
    /// Replace the resident PR observer's stable session set. The production
    /// adapter performs no RPC on this call; it only wakes its background lane.
    SyncPullRequestTargets {
        sessions: Vec<SessionId>,
    },
    /// List one filtered group of a target's repository files (`path: None`) or
    /// read one selected file (`path: Some`) through the overlay data owner.
    LoadPreview {
        target: Target,
        request_id: RequestId,
        path: Option<String>,
        filter: PreviewFileFilter,
    },
    /// Discard pending and in-flight preview work after leaving the overlay.
    CancelPreview,
    /// Open one already-selected Pull Request URL through the browser opener.
    /// URL validation stays with the executor; the reducer forwards the raw URL.
    OpenPullRequest {
        url: String,
    },
    /// Copy one selected canonical URL to the OS clipboard.
    CopyPullRequest {
        url: String,
    },
    /// Persist a user-owned dismissed tombstone for one session PR.
    DismissPullRequest {
        session: SessionId,
        url: String,
    },
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn update(state: &mut AppState, event: AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::WorkflowEdit { session, edit } => {
            if state.active != Some(session)
                || state.overlay.is_some()
                || state.workspace_drawer_focus.is_some()
                || state.route != Route::Home(HomeMode::Closeup)
                || !state.session_can_use(session)
            {
                return Vec::new();
            }
            if let Some(panel) = state.workflows.get_mut(&session) {
                if panel.run.is_none() && panel.agent_field.is_some() {
                    return Vec::new();
                }
                match edit {
                    super::workflow::WorkflowEdit::Start => panel.draft.move_edge(false),
                    super::workflow::WorkflowEdit::End => panel.draft.move_edge(true),
                    super::workflow::WorkflowEdit::Delete => panel.draft.delete_forward(),
                }
            }
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Workflow { job, result }) => {
            if job.workspace != state.workspace || !state.sessions.contains(&job.session) {
                return Vec::new();
            }
            let Some(panel) = state.workflows.get_mut(&job.session) else {
                return Vec::new();
            };
            if let Some(control) = &job.control {
                if panel.pending.as_ref() != Some(control) {
                    return Vec::new();
                }
                panel.submitting = false;
            } else {
                panel.loading = false;
            }
            match result {
                Ok(snapshot) if snapshot.session == job.session => {
                    if !panel.agents_edited && panel.pending.is_none() {
                        panel.agents = snapshot.agents;
                    }
                    if let Some(start) = snapshot.pending_start {
                        panel.agents = start.agents;
                        if panel.pending.is_none() {
                            if panel.draft.value().is_empty() {
                                panel.draft.paste(&start.goal);
                            }
                            panel.pending = Some((
                                start.operation_id,
                                usagi_core::domain::workflow::WorkflowCommand::Start {
                                    goal: start.goal,
                                    agents: start.agents,
                                },
                            ));
                        }
                        panel.error = start.error;
                    }
                    panel.run = snapshot.run;
                    if panel.pending.is_none() {
                        panel.error = None;
                    }
                    if let Some((_, command)) = job.control {
                        let body = match command {
                            usagi_core::domain::workflow::WorkflowCommand::Start {
                                goal, ..
                            } => goal,
                            usagi_core::domain::workflow::WorkflowCommand::Instruct {
                                body,
                                ..
                            } => body,
                        };
                        panel.submitted(&body);
                        panel.pending = None;
                    }
                }
                Ok(_) => panel.error = Some("Workflow response belongs to another session".into()),
                Err(error) => {
                    if job.control.is_some() && !error.unconfirmed {
                        panel.pending = None;
                    }
                    panel.error = Some(error.message);
                }
            }
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Decisions {
            workspace,
            decisions,
        }) => decision::update(
            state,
            decision::Event::Snapshot {
                workspace,
                decisions,
            },
        ),
        AppEvent::Backend(BackendEvent::DecisionResolved {
            workspace,
            decision_id,
        }) => decision::update(
            state,
            decision::Event::Resolved {
                workspace,
                decision_id,
            },
        ),
        AppEvent::Backend(BackendEvent::DecisionError {
            workspace,
            decision_id,
            error,
        }) => decision::update(
            state,
            decision::Event::Error {
                workspace,
                decision_id,
                error,
            },
        ),
        AppEvent::Backend(
            event @ (BackendEvent::NotesLoaded { .. }
            | BackendEvent::NotesError { .. }
            | BackendEvent::EnvironmentLoaded { .. }
            | BackendEvent::EnvironmentSaved { .. }
            | BackendEvent::EnvironmentError { .. }
            | BackendEvent::RolesLoaded { .. }
            | BackendEvent::RolesError { .. }
            | BackendEvent::PullRequestsLoaded { .. }
            | BackendEvent::PullRequestsError { .. }
            | BackendEvent::PreviewLoaded { .. }
            | BackendEvent::PreviewError { .. }),
        ) => {
            let _ = update_editor_backend(state, &event);
            Vec::new()
        }
        AppEvent::Key(key) => {
            state.pending_session_click = None;
            update_key(state, key)
        }
        AppEvent::WorkflowInput { session, key } => {
            if state.active != Some(session)
                || !state.sessions.contains(&session)
                || !state.session_can_use(session)
                || state.overlay.is_some()
                || state.workspace_drawer_focus().is_some()
                || state.route != Route::Home(HomeMode::Closeup)
            {
                return Vec::new();
            }
            let panel = state.workflows.entry(session).or_default();
            if panel.run.is_none() && panel.agent_field.is_some() {
                match key {
                    AppKey::Left => {
                        panel.cycle_agent(false);
                        return Vec::new();
                    }
                    AppKey::Right => {
                        panel.cycle_agent(true);
                        return Vec::new();
                    }
                    AppKey::Tab | AppKey::SaveRoles => {}
                    _ => return Vec::new(),
                }
            }
            match key {
                AppKey::Char(character) => panel.draft.insert(&character.to_string()),
                AppKey::Paste(text) => panel.draft.paste(&text),
                AppKey::Enter => panel.draft.newline(),
                AppKey::Backspace => panel.draft.backspace(),
                AppKey::Left => panel.draft.move_cursor(false),
                AppKey::Right => panel.draft.move_cursor(true),
                AppKey::Up => panel.draft.move_vertical(false),
                AppKey::Down => panel.draft.move_vertical(true),
                AppKey::Tab => panel.cycle_recipient(),
                AppKey::PageUp => panel.history_offset = panel.history_offset.saturating_add(5),
                AppKey::PageDown => panel.history_offset = panel.history_offset.saturating_sub(5),
                AppKey::SaveRoles => {
                    if panel.loading || panel.submitting {
                        return Vec::new();
                    }
                    if panel.pending.is_none() {
                        let body = panel.draft.value().to_owned();
                        if body.trim().is_empty() || body.len() > 16 * 1024 || body.contains('\0') {
                            panel.error =
                                Some("Enter a non-empty instruction of at most 16 KiB".into());
                            return Vec::new();
                        }
                        let command = if panel.run.is_some() {
                            usagi_core::domain::workflow::WorkflowCommand::Instruct {
                                recipient: panel
                                    .recipient
                                    .unwrap_or(usagi_core::domain::workflow::Recipient::Automatic),
                                body,
                            }
                        } else {
                            usagi_core::domain::workflow::WorkflowCommand::Start {
                                goal: body,
                                agents: panel.agents,
                            }
                        };
                        panel.pending = Some((OperationId::new(), command));
                    }
                    panel.submitting = true;
                    panel.error = None;
                    return vec![Effect::Workflow(super::workflow::WorkflowJob {
                        workspace: state.workspace,
                        session,
                        control: panel.pending.clone(),
                    })];
                }
                _ => {}
            }
            Vec::new()
        }
        AppEvent::RetainedPaneActivated(target) => {
            let Target::Session(session) = target else {
                return Vec::new();
            };
            if state.selected != Selection::Target(target)
                || !state.sessions.contains(&session)
                || state.session_can_use(session)
            {
                return Vec::new();
            }
            state.active = Some(session);
            state.route = Route::Home(HomeMode::Closeup);
            state.closeup_action_forced = false;
            state.overlay = None;
            Vec::new()
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
            if state.workspace_drawer_open() {
                return Vec::new();
            }
            if matches!(state.route, Route::Home(HomeMode::Closeup))
                && has_live_pane
                && !state.closeup_action_forced
            {
                state.overlay = None;
            }
            Vec::new()
        }
        AppEvent::PaneTabAvailability {
            available: has_pane_tab,
            error,
        } => {
            if has_pane_tab == state.has_pane_tab {
                return Vec::new();
            }
            state.has_pane_tab = has_pane_tab;
            if state.workspace_drawer_open() {
                return Vec::new();
            }
            if !matches!(state.route, Route::Home(HomeMode::Closeup)) {
                return Vec::new();
            }
            if has_pane_tab {
                // Only the launcher steps aside: a forced action modal the user
                // opened explicitly, and every other overlay, stay as they are.
                if !state.closeup_action_forced && state.overlay == Some(Overlay::Closeup) {
                    state.overlay = None;
                }
            } else if !state.has_live_pane && state.overlay.is_none() {
                // A pane that failed to launch (rather than exiting cleanly)
                // carries a safe reason; retain it on the empty Closeup so the
                // failed shortcut is not indistinguishable from a no-op.
                if let Some(message) = error {
                    state.notice = Some(Notice::new(message));
                }
            }
            Vec::new()
        }
        AppEvent::TerminalLaunchFailed(error) => {
            state.notice = Some(error.clone());
            if state.overlay.is_none() {
                state.terminal_launch_error = Some(error);
                state.overlay = Some(Overlay::TerminalLaunchError);
            }
            Vec::new()
        }
        AppEvent::AgentLaunchFailed(error) => {
            state.notice = Some(error.clone());
            if state.overlay.is_none() {
                state.agent_launch_error = Some(error);
                state.overlay = Some(Overlay::AgentLaunchError);
            }
            Vec::new()
        }
        AppEvent::Resize { width, height } => {
            // The frame loop re-applies the real terminal size every frame, so
            // this arrives as a *level*. Only its edge closes the Garden: a
            // screen saver must not survive a resize the user just performed,
            // but an unchanged re-sample is inert.
            if state.overlay == Some(Overlay::Garden)
                && state
                    .size
                    .is_some_and(|previous| previous != (width, height))
            {
                state.overlay = None;
            }
            state.size = Some((width, height));
            Vec::new()
        }
        AppEvent::GardenAvailability(available) => {
            state.garden_available = available;
            if !available && state.overlay == Some(Overlay::Garden) {
                state.overlay = None;
            }
            Vec::new()
        }
        AppEvent::Pointer { column, row, at } => update_pointer(state, column, row, at),
        AppEvent::IdleElapsed(elapsed) => update_idle(state, elapsed),
        AppEvent::GardenClick(click) => update_garden_click(state, click),
        AppEvent::FocusSession(session) => focus_session(state, session),
        AppEvent::VisitSession(session) => visit_session(state, session),
        AppEvent::GardenUnavailable => {
            if state.overlay == Some(Overlay::Garden) {
                state.overlay = None;
                state.notice = Some(Notice::new(
                    "garden is unavailable at the current terminal size",
                ));
            }
            Vec::new()
        }
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
            state
                .pr_merge_celebrations
                .retain(|_, until| state.mascot_tick <= *until);
            if state.mascot_tick.is_multiple_of(10)
                && let Some(session) = state.active
                && state.session_can_use(session)
                && let Some(panel) = state.workflows.get_mut(&session)
                && !panel.loading
                && !panel.submitting
            {
                panel.loading = true;
                vec![Effect::Workflow(super::workflow::WorkflowJob {
                    workspace: state.workspace,
                    session,
                    control: None,
                })]
            } else {
                Vec::new()
            }
        }
        AppEvent::Backend(BackendEvent::Sessions(sessions)) => {
            // Never combine a press from before an authoritative snapshot with
            // one after it, even when the same stable ID remains visible.
            state.pending_session_click = None;
            let previous_sessions = std::mem::replace(&mut state.sessions, sessions);
            state
                .workflows
                .retain(|session, _| state.sessions.contains(session));
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
            let before = state.session_prs.len();
            state
                .session_prs
                .retain(|session, _| state.sessions.contains(session));
            if state.session_prs.len() != before {
                state.session_pr_revision = state.session_pr_revision.saturating_add(1);
            }
            if state.pr_overlay.as_ref().is_some_and(|overlay| {
                matches!(overlay.target, Target::Session(session) if !state.sessions.contains(&session))
            }) {
                state.pr_overlay = None;
                if state.overlay == Some(Overlay::Prs) {
                    state.overlay = None;
                }
            }
            state.reconcile_sessions(&previous_sessions);
            reconcile_force_remove_confirmation(state);
            let mut effects = vec![Effect::SyncPullRequestTargets {
                sessions: state.sessions.clone(),
            }];
            effects.extend(continue_cleanup_after_snapshot(state));
            effects.extend(continue_remove_after_snapshot(state));
            effects
        }
        AppEvent::Backend(BackendEvent::SessionNames(names)) => {
            state.session_names = names;
            if let Some(form) = state.create_session.as_mut() {
                form.replace_existing(&state.session_names);
            }
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::SessionLifecycles(lifecycles)) => {
            state.session_lifecycles = lifecycles;
            let sessions = state.sessions.clone();
            state.reconcile_sessions(&sessions);
            reconcile_force_remove_confirmation(state);
            let failed = state
                .cleanup_queue
                .as_ref()
                .and_then(CleanupQueueState::in_flight)
                .filter(|session| {
                    state
                        .session_lifecycles
                        .get(session)
                        .is_some_and(|projection| {
                            projection.lifecycle == SessionLifecycle::Failed
                                && projection.failure_stage == Some(FailureStage::Delete)
                        })
                });
            if let Some(failed) = failed
                && let Some(queue) = state.cleanup_queue.as_mut()
            {
                queue.in_flight = None;
                queue.selected.remove(&failed);
                queue.feedback = Some(Notice::new("cleanup paused after removal failed"));
            }
            let failed = state
                .remove_queue
                .as_ref()
                .and_then(RemoveQueueState::in_flight)
                .filter(|session| {
                    state
                        .session_lifecycles
                        .get(session)
                        .is_some_and(|projection| {
                            projection.lifecycle == SessionLifecycle::Failed
                                && projection.failure_stage == Some(FailureStage::Delete)
                        })
                });
            if let Some(failed) = failed
                && let Some(queue) = state.remove_queue.as_mut()
            {
                queue.in_flight = None;
                queue.selected.remove(&failed);
                queue.feedback = Some(Notice::new("removal paused after a session failed"));
            }
            reconcile_cleanup_queue(state);
            reconcile_remove_queue(state);
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::SessionRoles(roles)) => {
            state.session_roles = roles;
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::SessionRoleCatalog(catalog)) => {
            state.role_catalog = catalog;
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::SessionBranchCatalog(catalog)) => {
            state.branch_catalog = catalog;
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::DaemonControlFinished {
            workspace,
            action,
            token,
            result,
        }) => {
            if workspace != state.workspace || state.daemon_control.pending != Some((action, token))
            {
                return Vec::new();
            }
            state.daemon_control.pending = None;
            state.notice = Some(match &result {
                Ok(notice) => notice.clone(),
                Err(error) => Notice::new(error.message.as_str()),
            });
            state.daemon_control.result = Some(result);
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Notice(notice)) => {
            if let Some(queue) = state.cleanup_queue.as_mut()
                && let Some(failed) = queue.in_flight.take()
            {
                queue.selected.remove(&failed);
                queue.feedback = Some(notice.clone());
            }
            if let Some(queue) = state.remove_queue.as_mut()
                && let Some(failed) = queue.in_flight.take()
            {
                queue.selected.remove(&failed);
                queue.feedback = Some(notice.clone());
            }
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
            if let Some(entry) = state
                .runtimes
                .iter_mut()
                .find(|entry| entry.runtime.fences(&runtime))
            {
                entry.phase = phase;
            } else {
                state.runtimes.push(RuntimePhase { runtime, phase });
            }
            reconcile_cleanup_queue(state);
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Feedback(feedback)) => {
            let refresh_prs = matches!(feedback, Feedback::Reconnected | Feedback::ResyncRequired);
            state.feedback = Some(feedback);
            if refresh_prs {
                vec![Effect::SyncPullRequestTargets {
                    sessions: state.sessions.clone(),
                }]
            } else {
                Vec::new()
            }
        }
        AppEvent::DirectorLaunchFinished {
            operation,
            supervisor_run_id,
            succeeded,
        } => {
            if state.director_launching == Some(operation) {
                state.director_launching = None;
                if succeeded {
                    state.director_route = match (state.work_mode, supervisor_run_id) {
                        (WorkMode::GoalDriven, Some(run)) => DirectorRoute::RunOverview(run),
                        (WorkMode::Classic, None) => {
                            DirectorRoute::Console(DirectorConsoleParent::Organization)
                        }
                        // The launch belongs to the workflow active when it
                        // was submitted. A later workflow switch keeps the
                        // daemon-owned result alive without crossing the two
                        // Director screen trees.
                        (mode, _) => DirectorRoute::landing_for(mode),
                    };
                }
            }
            Vec::new()
        }
        AppEvent::RootTerminalDrawerEmptied => {
            state.root_terminal_drawer_open = false;
            state.root_terminal_full_height = false;
            if state.workspace_drawer_focus == Some(WorkspaceDrawerFocus::Terminal) {
                state.workspace_drawer_focus = state
                    .director_drawer_open
                    .then_some(WorkspaceDrawerFocus::Director);
            }
            Vec::new()
        }
        AppEvent::DirectorDrawerEmptied => {
            state.director_drawer_open = false;
            state.director_new = DirectorNew::Idle;
            state.director_goal.clear();
            if state.workspace_drawer_focus == Some(WorkspaceDrawerFocus::Director) {
                state.workspace_drawer_focus = state
                    .root_terminal_drawer_open
                    .then_some(WorkspaceDrawerFocus::Terminal);
            }
            Vec::new()
        }
        AppEvent::WorkspaceDrawerFocused(focus) => {
            let open = match focus {
                WorkspaceDrawerFocus::Director => state.director_drawer_open,
                WorkspaceDrawerFocus::Terminal => state.root_terminal_drawer_open,
            };
            if open {
                if focus == WorkspaceDrawerFocus::Director {
                    state.root_terminal_full_height = false;
                }
                state.workspace_drawer_focus = Some(focus);
            }
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
                    state.active = Some(created);
                    state.route = Route::Home(HomeMode::Closeup);
                    state.closeup_action_forced = false;
                    state.overlay = None;
                }
            } else if pending.is_some_and(|pending| pending.kind == PendingKind::CreateSession)
                && state.overlay.is_none()
                && !state.workspace_drawer_open()
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

#[allow(clippy::too_many_lines)] // Exhaustive reflux routing keeps every editor completion fenced in one match.
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
        BackendEvent::EnvironmentLoaded {
            scope,
            entries,
            inherited: _,
        } => {
            if let Some(editor) = state
                .environment_editor
                .as_mut()
                .filter(|editor| editor.scope == *scope)
            {
                editor.entries.clone_from(entries);
                let bindings = entries
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.value.clone()))
                    .collect::<EnvBindings>();
                editor.source.replace(format_env_bindings(&bindings));
                editor.error = None;
                editor.loading = false;
                editor.saving = false;
            }
        }
        BackendEvent::EnvironmentSaved {
            scope,
            entries,
            inherited: _,
        } => {
            let close_after_save = state
                .environment_editor
                .as_ref()
                .is_some_and(|editor| editor.scope == *scope && editor.saving);
            if close_after_save {
                state.overlay = None;
                state.environment_editor = None;
            } else if let Some(editor) = state
                .environment_editor
                .as_mut()
                .filter(|editor| editor.scope == *scope)
            {
                editor.entries.clone_from(entries);
                editor.error = None;
                editor.loading = false;
                editor.saving = false;
            }
        }
        BackendEvent::EnvironmentError { scope, error } => {
            if let Some(editor) = state
                .environment_editor
                .as_mut()
                .filter(|editor| editor.scope == *scope)
            {
                editor.error = Some(error.clone());
                editor.loading = false;
                editor.saving = false;
            }
        }
        BackendEvent::RolesLoaded { scope, source } => {
            if let Some(editor) = state
                .role_editor
                .as_mut()
                .filter(|editor| editor.scope == *scope)
            {
                editor.source.clone_from(source);
                editor.follow_tail();
                editor.error = None;
                editor.loading = false;
                editor.saving = false;
            }
        }
        BackendEvent::RolesError { scope, error } => {
            if let Some(editor) = state
                .role_editor
                .as_mut()
                .filter(|editor| editor.scope == *scope)
            {
                editor.error = Some(error.clone());
                editor.loading = false;
                editor.saving = false;
            }
        }
        BackendEvent::PullRequestsLoaded {
            target,
            revision,
            prs,
        } => {
            let mut newly_detected = None;
            let accepted = match target {
                Target::Root(_) => true,
                Target::Session(session) => {
                    let current = state.session_prs.get(session);
                    let accepted = state.sessions.contains(session)
                        && current.is_none_or(|(current, _)| *revision > *current);
                    if accepted {
                        // The first authoritative snapshot establishes the
                        // baseline. Only a URL added by a later revision is a
                        // live discovery that should interrupt Home.
                        newly_detected = current.and_then(|(_, current)| {
                            prs.iter().position(|pr| {
                                pr.state != PrState::Dismissed
                                    && pr.auto_open
                                    && current.iter().all(|known| known.identity != pr.identity)
                            })
                        });
                        let newly_merged = current.is_some_and(|(_, current)| {
                            prs.iter().any(|pr| {
                                pr.state == PrState::Merged
                                    && current.iter().any(|known| {
                                        known.identity == pr.identity
                                            && known.state != PrState::Merged
                                    })
                            })
                        });
                        if newly_merged {
                            state
                                .pr_merge_celebrations
                                .insert(*session, state.mascot_tick.saturating_add(24));
                        }
                        state.session_prs.insert(*session, (*revision, prs.clone()));
                        state.session_pr_revision = state.session_pr_revision.saturating_add(1);
                    }
                    accepted
                }
            };
            if let Some(overlay) = state
                .pr_overlay
                .as_mut()
                .filter(|overlay| accepted && overlay.target == *target)
            {
                overlay.prs = filtered_prs(prs, overlay.filter);
                let detected_selection = newly_detected.and_then(|index| {
                    prs.get(index).and_then(|detected| {
                        overlay
                            .prs
                            .iter()
                            .position(|pr| pr.identity == detected.identity)
                    })
                });
                overlay.selected = detected_selection
                    .unwrap_or(overlay.selected)
                    .min(overlay.prs.len().saturating_sub(1));
                overlay.error = None;
            }
            // An explicit `p` request is kept as a hidden pending overlay until
            // its snapshot arrives. Only an inventory with no visible PR at all
            // closes the modal. A status tab with no matches remains open so the
            // user can navigate to another tab without reopening the inventory.
            if state
                .pr_overlay
                .as_ref()
                .is_some_and(|overlay| overlay.target == *target)
            {
                let has_visible_prs = match target {
                    Target::Root(_) => !filtered_prs(prs, PrFilter::All).is_empty(),
                    Target::Session(session) => state
                        .session_prs(*session)
                        .is_some_and(|prs| !filtered_prs(prs, PrFilter::All).is_empty()),
                };
                if !has_visible_prs {
                    state.pr_overlay = None;
                    if state.overlay == Some(Overlay::Prs) {
                        state.overlay = None;
                    }
                } else if state.overlay.is_none() && !state.workspace_drawer_open() {
                    state.overlay = Some(Overlay::Prs);
                } else if state.overlay != Some(Overlay::Prs) {
                    // Do not let a delayed explicit request steal a newer
                    // foreground interaction.
                    state.pr_overlay = None;
                }
            }
            // A freshly discovered PR is the completion of work the user is
            // waiting for, so surface it immediately. Metadata-only refreshes,
            // duplicate snapshots, and deliberate dismissals stay quiet. An
            // existing modal or Director interaction remains the input owner.
            let may_auto_open = match state.pr_auto_open {
                PrAutoOpen::Always => true,
                PrAutoOpen::SwitchOnly => {
                    matches!(state.route, Route::Home(HomeMode::Switch))
                }
                PrAutoOpen::NotifyOnly | PrAutoOpen::Never => false,
            };
            if accepted
                && let Some(selected) = newly_detected
                && may_auto_open
                && state.overlay.is_none()
                && !state.workspace_drawer_open()
            {
                let detected_identity = prs.get(selected).map(|pr| &pr.identity);
                let visible = filtered_prs(prs, PrFilter::All);
                let visible_selected = detected_identity
                    .and_then(|identity| visible.iter().position(|pr| &pr.identity == identity))
                    .unwrap_or(0);
                state.overlay = Some(Overlay::Prs);
                state.pr_overlay = Some(PrOverlay {
                    target: *target,
                    prs: visible,
                    selected: visible_selected,
                    error: None,
                    filter: PrFilter::All,
                });
                state.preview_overlay = None;
            } else if accepted
                && let Some(selected) = newly_detected
                && state.pr_auto_open == PrAutoOpen::NotifyOnly
                && let Some(pr) = prs.get(selected)
            {
                state.notice = Some(Notice::new(format!("PR detected: {}", pr.url())));
            }
            reconcile_cleanup_queue(state);
        }
        BackendEvent::PullRequestsError { target, error } => {
            let matching_request = state
                .pr_overlay
                .as_ref()
                .is_some_and(|overlay| overlay.target == *target);
            if matching_request {
                if state.overlay.is_none() && !state.workspace_drawer_open() {
                    state.overlay = Some(Overlay::Prs);
                } else if state.overlay != Some(Overlay::Prs) {
                    state.pr_overlay = None;
                }
                if let Some(overlay) = state.pr_overlay.as_mut() {
                    overlay.error = Some(error.clone());
                }
            }
        }
        BackendEvent::PreviewLoaded {
            target,
            request_id,
            path,
            filter,
            files,
            lines,
        } => {
            if let Some(overlay) = state.preview_overlay.as_mut().filter(|overlay| {
                overlay.target == *target
                    && overlay.request_id == *request_id
                    && overlay.path.as_ref() == path.as_ref()
                    && overlay.file_filter == *filter
            }) {
                if path.is_none() {
                    overlay.files = files
                        .iter()
                        .filter(|path| presentation_text_is_safe(path))
                        .cloned()
                        .collect();
                    overlay.selected = 0;
                } else {
                    overlay.lines = lines
                        .iter()
                        .map(|line| sanitize_presentation_line(line))
                        .collect();
                    overlay.scroll = 0;
                    overlay.search.clear();
                    overlay.search_editing = false;
                    overlay.current_match = 0;
                }
                overlay.loading = false;
                overlay.error = None;
            }
        }
        BackendEvent::PreviewError {
            target,
            request_id,
            path,
            filter,
            error,
        } => {
            if let Some(overlay) = state.preview_overlay.as_mut().filter(|overlay| {
                overlay.target == *target
                    && overlay.request_id == *request_id
                    && overlay.path.as_ref() == path.as_ref()
                    && overlay.file_filter == *filter
            }) {
                overlay.loading = false;
                overlay.error = Some(error.clone());
            }
        }
        _ => return false,
    }
    true
}

fn update_key(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    state.interaction_count = state.interaction_count.saturating_add(1);
    // Existing modal overlays have precedence even if an asynchronous
    // completion opened one while the drawer was already visible.
    if let Some(overlay) = state.overlay {
        return update_overlay(state, overlay, key);
    }
    if state.workspace_drawer_focus == Some(WorkspaceDrawerFocus::Director) {
        return update_director_drawer_key(state, key);
    }
    if state.workspace_drawer_focus == Some(WorkspaceDrawerFocus::Terminal) {
        return update_root_terminal_drawer_key(state, &key);
    }
    if matches!(key, AppKey::ToggleDirectorDrawer) {
        state.director_drawer_open = true;
        state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Director);
        state.director_new = DirectorNew::Idle;
        state.director_goal.clear();
        return Vec::new();
    }
    if matches!(key, AppKey::OpenDirectorWorkRuns)
        && state.work_mode == WorkMode::GoalDriven
        && state.director_launching.is_none()
        && matches!(state.director_new, DirectorNew::Idle)
    {
        state.director_drawer_open = true;
        state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Director);
        state.director_route = DirectorRoute::WorkRuns;
        state.director_new = DirectorNew::Idle;
        state.director_goal.clear();
        return Vec::new();
    }
    if matches!(key, AppKey::OpenDirectorNew) {
        state.director_drawer_open = true;
        state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Director);
        open_director_new(state);
        return Vec::new();
    }
    if matches!(key, AppKey::ToggleRootTerminalDrawer) {
        state.root_terminal_drawer_open = true;
        state.root_terminal_full_height = false;
        state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Terminal);
        return vec![Effect::OpenTerminal {
            target: Target::Root(state.workspace),
            operation_id: OperationId::new(),
            arguments: "open".to_owned(),
        }];
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
                state.exit_choice = ExitChoice::Quit;
                state.overlay = Some(Overlay::QuitConfirmation);
                Vec::new()
            } else {
                vec![Effect::Detach]
            }
        }
        AppKey::CtrlQ | AppKey::OpenQuitConfirmation => {
            state.exit_choice = ExitChoice::Quit;
            state.overlay = Some(Overlay::QuitConfirmation);
            Vec::new()
        }
        AppKey::OpenNotes | AppKey::OpenEnvironment => {
            update_editor_key(state, &key).unwrap_or_default()
        }
        key => update_management_key(state, key),
    }
}

/// Terminal rows the TUI assumes when it has not observed a size yet. Mirrors
/// `widgets::normalize_size`; the assertion there keeps the two agreeing.
pub(crate) const NORMALIZED_TERMINAL_ROWS: usize = 24;
/// Rows the director drawer spends on chrome around the launch picker's candidate
/// rows: the Home header above the drawer, the panel's top and bottom borders,
/// two vertical padding rows, the route breadcrumb, its separator, and the
/// footer hint.
/// Mirrors `views::director_drawer`'s `PICKER_CHROME_ROWS`; the assertion there
/// keeps the launch gate and the render agreeing on the geometry.
pub(crate) const DIRECTOR_PICKER_CHROME_ROWS: usize = 8;
/// Goal label, input, and provider label consume three additional rows before
/// Goal Composer can show the selected provider.
pub(crate) const DIRECTOR_GOAL_COMPOSER_CHROME_ROWS: usize = DIRECTOR_PICKER_CHROME_ROWS + 3;

/// Candidate rows the launch picker can draw at `height` terminal rows.
///
/// This is the launch gate's half of the picker geometry: a terminal too short
/// for a single candidate row shows no highlighted CLI, so Enter must not mint
/// a launch the operator never saw.
#[must_use]
pub(crate) fn director_picker_capacity(height: usize) -> usize {
    let height = if height == 0 {
        NORMALIZED_TERMINAL_ROWS
    } else {
        height
    };
    height.saturating_sub(DIRECTOR_PICKER_CHROME_ROWS)
}

/// Provider rows the Goal Composer can draw at `height` terminal rows.
#[must_use]
pub(crate) fn director_goal_composer_picker_capacity(height: usize) -> usize {
    let height = if height == 0 {
        NORMALIZED_TERMINAL_ROWS
    } else {
        height
    };
    height.saturating_sub(DIRECTOR_GOAL_COMPOSER_CHROME_ROWS)
}

/// Whether the drawer can currently draw the highlighted candidate row. An
/// unobserved terminal size keeps the picker usable: the renderer normalizes the
/// same way, so the first frame is never gated on a resize event.
fn director_picker_shows_selection(state: &AppState) -> bool {
    state.size.is_none_or(|(_, height)| {
        let height = usize::from(height);
        if state.work_mode == WorkMode::GoalDriven {
            director_goal_composer_picker_capacity(height) > 0
        } else {
            director_picker_capacity(height) > 0
        }
    })
}

/// Unicode bidi controls can reorder surrounding labels without being visible.
/// They are not accepted in a single-field terminal composer even though Rust
/// does not classify every one of them as a control character.
const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Append one paste/key stream to the Goal `SSoT`.
///
/// Goal Composer is a single logical field: line-breaking controls become one
/// visible separator, all other terminal/control and bidi formatting bytes are
/// discarded, and the daemon's byte limit is applied on UTF-8 boundaries.
fn append_goal_text(goal: &mut String, characters: impl IntoIterator<Item = char>) {
    let mut normalized_separator = false;
    for character in characters {
        let character = if character == ' ' {
            if normalized_separator {
                continue;
            }
            character
        } else if character.is_whitespace() {
            normalized_separator = true;
            if goal.chars().last().is_some_and(char::is_whitespace) {
                continue;
            }
            ' '
        } else if character.is_control() || is_bidi_control(character) {
            continue;
        } else {
            normalized_separator = false;
            character
        };
        if goal.len() + character.len_utf8() > MAX_WORK_GOAL_BYTES {
            break;
        }
        goal.push(character);
    }
}

fn update_goal_composer_text(state: &mut AppState, key: &AppKey) -> bool {
    if state.work_mode != WorkMode::GoalDriven
        || !matches!(state.director_new, DirectorNew::Choosing(_))
    {
        return false;
    }
    match &key {
        AppKey::Backspace => {
            state.director_goal.pop();
        }
        AppKey::Char(character) => append_goal_text(&mut state.director_goal, [*character]),
        AppKey::Paste(value) => {
            append_goal_text(&mut state.director_goal, value.chars());
        }
        _ => return false,
    }
    true
}

fn update_director_drawer_key(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    if let Some(effects) = update_director_shell_key(state, &key) {
        return effects;
    }
    if update_director_route_key(state, &key) {
        return Vec::new();
    }
    if update_goal_composer_text(state, &key) {
        return Vec::new();
    }
    match (state.director_new, key) {
        (DirectorNew::Idle, AppKey::OpenDirectorNew) => {
            open_director_new(state);
            Vec::new()
        }
        (DirectorNew::Idle, AppKey::Escape) => {
            if state.director_route == DirectorRoute::landing_for(state.work_mode) {
                state.director_drawer_open = false;
                state.workspace_drawer_focus = state
                    .root_terminal_drawer_open
                    .then_some(WorkspaceDrawerFocus::Terminal);
            } else {
                director_back(state);
            }
            Vec::new()
        }
        (DirectorNew::Choosing(_) | DirectorNew::Empty, AppKey::Escape | AppKey::CtrlC) => {
            state.director_new = DirectorNew::Idle;
            state.director_goal.clear();
            Vec::new()
        }
        (DirectorNew::Choosing(selected), AppKey::Up) => {
            let candidates = state.available_models.iter().collect::<Vec<_>>();
            if candidates.is_empty() {
                state.director_new = DirectorNew::Empty;
                return Vec::new();
            }
            let index = candidates
                .iter()
                .position(|candidate| *candidate == selected)
                .unwrap_or(0);
            let previous = (index + candidates.len() - 1) % candidates.len();
            state.director_new = DirectorNew::Choosing(candidates[previous]);
            Vec::new()
        }
        (DirectorNew::Choosing(selected), AppKey::Down) => {
            let candidates = state.available_models.iter().collect::<Vec<_>>();
            if candidates.is_empty() {
                state.director_new = DirectorNew::Empty;
                return Vec::new();
            }
            let index = candidates
                .iter()
                .position(|candidate| *candidate == selected)
                .unwrap_or(0);
            let next = (index + 1) % candidates.len();
            state.director_new = DirectorNew::Choosing(candidates[next]);
            Vec::new()
        }
        (DirectorNew::Choosing(selected), AppKey::Enter)
            if state.director_launching.is_none()
                && director_picker_shows_selection(state)
                && (state.work_mode == WorkMode::Classic
                    || !state.director_goal.trim().is_empty()) =>
        {
            let operation_id = OperationId::new();
            state.director_new = DirectorNew::Idle;
            state.director_launching = Some(operation_id);
            if state.work_mode == WorkMode::GoalDriven {
                vec![Effect::LaunchGoal {
                    workspace: state.workspace,
                    operation_id,
                    profile: Some(profile_for(selected)),
                    goal: std::mem::take(&mut state.director_goal),
                }]
            } else {
                vec![Effect::LaunchAgent {
                    workspace: state.workspace,
                    session: None,
                    operation_id,
                    profile: Some(profile_for(selected)),
                }]
            }
        }
        // Empty, submitted, and unsupported drawer input are all inert. In
        // particular Enter while a root launch is fenced cannot mint a second
        // operation, and Enter on a terminal too short to draw the highlighted
        // candidate cannot launch a CLI the operator never saw.
        _ => Vec::new(),
    }
}

fn update_director_shell_key(state: &mut AppState, key: &AppKey) -> Option<Vec<Effect>> {
    if matches!(key, AppKey::ToggleRootTerminalDrawer) {
        state.root_terminal_drawer_open = true;
        state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Terminal);
        return Some(vec![Effect::OpenTerminal {
            target: Target::Root(state.workspace),
            operation_id: OperationId::new(),
            arguments: "open".to_owned(),
        }]);
    }
    if matches!(key, AppKey::ToggleDirectorDrawer) {
        state.director_drawer_open = false;
        state.workspace_drawer_focus = state
            .root_terminal_drawer_open
            .then_some(WorkspaceDrawerFocus::Terminal);
        state.director_new = DirectorNew::Idle;
        state.director_goal.clear();
        return Some(Vec::new());
    }
    (state.director_launching.is_some() && matches!(key, AppKey::Escape)).then(Vec::new)
}

fn update_director_route_key(state: &mut AppState, key: &AppKey) -> bool {
    match key {
        AppKey::OpenDirectorOrganization => {
            if state.work_mode != WorkMode::Classic {
                return true;
            }
            state.director_route = DirectorRoute::Organization;
            state.director_new = DirectorNew::Idle;
            state.director_goal.clear();
            true
        }
        AppKey::OpenDirectorWorkRuns => {
            if state.work_mode != WorkMode::GoalDriven
                || state.director_launching.is_some()
                || !matches!(state.director_new, DirectorNew::Idle)
            {
                return true;
            }
            state.director_route = DirectorRoute::WorkRuns;
            state.director_new = DirectorNew::Idle;
            state.director_goal.clear();
            true
        }
        AppKey::OpenDirectorRunOverview(run) => {
            if state.work_mode != WorkMode::GoalDriven {
                return true;
            }
            state.director_route = DirectorRoute::RunOverview(*run);
            state.director_new = DirectorNew::Idle;
            state.director_goal.clear();
            true
        }
        AppKey::OpenDirectorConsole(parent) => {
            let route = DirectorRoute::Console(*parent);
            if !route.belongs_to(state.work_mode) {
                return true;
            }
            state.director_route = route;
            state.director_new = DirectorNew::Idle;
            state.director_goal.clear();
            true
        }
        AppKey::DirectorBack => {
            if state.director_launching.is_none() {
                director_back(state);
            }
            true
        }
        _ => false,
    }
}

fn director_back(state: &mut AppState) {
    if !matches!(state.director_new, DirectorNew::Idle) {
        state.director_new = DirectorNew::Idle;
        state.director_goal.clear();
        return;
    }
    state.director_route = match (state.work_mode, state.director_route) {
        (WorkMode::Classic, DirectorRoute::Console(DirectorConsoleParent::Organization)) => {
            DirectorRoute::Organization
        }
        (WorkMode::GoalDriven, DirectorRoute::RunOverview(_)) => DirectorRoute::WorkRuns,
        (WorkMode::GoalDriven, DirectorRoute::Console(DirectorConsoleParent::RunOverview(run))) => {
            DirectorRoute::RunOverview(run)
        }
        (mode, route) if route == DirectorRoute::landing_for(mode) => route,
        // Normalize impossible or stale cross-workflow routes instead of
        // exposing the other workflow's screen tree.
        (mode, _) => DirectorRoute::landing_for(mode),
    };
}

fn open_director_new(state: &mut AppState) {
    if state.director_launching.is_some() {
        return;
    }
    state.director_goal.clear();
    state.director_new = if state.available_models.is_empty() {
        DirectorNew::Empty
    } else {
        let selected = if state.available_models.contains(state.default_model) {
            state.default_model
        } else {
            state
                .available_models
                .iter()
                .next()
                .expect("non-empty availability has a first candidate")
        };
        DirectorNew::Choosing(selected)
    };
}

/// Close the exit prompt and hand its answer to the backend. The overlay always
/// closes, so a committed answer never leaves the prompt on screen even when the
/// answer itself is "stay".
fn commit_exit_choice(state: &mut AppState, choice: ExitChoice) -> Vec<Effect> {
    state.overlay = None;
    state.exit_choice = choice;
    choice.effects()
}

/// Close the failed-delete prompt and, only for an affirmative answer, retry
/// the exact stable session with force. A refreshed/missing target fails closed.
fn commit_force_remove(state: &mut AppState, confirmed: bool) -> Vec<Effect> {
    let target = state
        .force_remove_confirmation
        .take()
        .map(|(session, _)| session);
    state.overlay = None;
    let Some(session) = target.filter(|session| {
        state.sessions.contains(session)
            && state
                .session_lifecycles
                .get(session)
                .is_some_and(|projection| {
                    projection.lifecycle == SessionLifecycle::Failed
                        && projection.failure_stage == Some(FailureStage::Delete)
                })
    }) else {
        return Vec::new();
    };
    if !confirmed {
        return Vec::new();
    }
    vec![Effect::RemoveSession {
        workspace: state.workspace,
        session,
        force: true,
        force_delete_branch: true,
        purge_orphan: false,
    }]
}

/// Drop a confirmation as soon as its daemon-authoritative delete failure is no
/// longer present. This prevents a stale modal from retargeting a refreshed row.
fn reconcile_force_remove_confirmation(state: &mut AppState) {
    let Some((session, _)) = state.force_remove_confirmation else {
        return;
    };
    let still_failed_delete = state.sessions.contains(&session)
        && state
            .session_lifecycles
            .get(&session)
            .is_some_and(|projection| {
                projection.lifecycle == SessionLifecycle::Failed
                    && projection.failure_stage == Some(FailureStage::Delete)
            });
    if still_failed_delete {
        return;
    }
    state.force_remove_confirmation = None;
    if state.overlay == Some(Overlay::ForceRemoveConfirmation) {
        state.overlay = None;
    }
}

fn dismiss_closeup_action_modal(state: &mut AppState) {
    state.closeup_action_forced = false;
    state.overlay = None;
}

fn cleanup_candidates(state: &AppState) -> Vec<SessionId> {
    let in_flight = state
        .cleanup_queue
        .as_ref()
        .and_then(CleanupQueueState::in_flight);
    state
        .sessions
        .iter()
        .copied()
        .filter(|session| Some(*session) == in_flight || state.session_can_cleanup(*session))
        .collect()
}

/// Rebuild only queue membership. Checked identities survive while eligible;
/// newly merged sessions join in workspace order, and stale rows disappear.
fn reconcile_cleanup_queue(state: &mut AppState) {
    let candidates = cleanup_candidates(state);
    let Some(queue) = state.cleanup_queue.as_mut() else {
        return;
    };
    queue.candidates = candidates;
    queue
        .selected
        .retain(|session| queue.candidates.contains(session));
    queue.cursor = queue.cursor.min(queue.candidates.len().saturating_sub(1));
}

fn begin_next_cleanup(state: &mut AppState, continuing: bool) -> Vec<Effect> {
    if let Some(queue) = state.cleanup_queue.as_mut()
        && queue.in_flight.is_some()
    {
        queue.feedback = Some(Notice::new("waiting for the current removal"));
        return Vec::new();
    }
    reconcile_cleanup_queue(state);
    let next =
        state.cleanup_queue.as_ref().and_then(|queue| {
            queue.candidates.iter().copied().find(|session| {
                queue.selected.contains(session) && state.session_can_cleanup(*session)
            })
        });
    let Some(session) = next else {
        if let Some(queue) = state.cleanup_queue.as_mut() {
            queue.feedback = Some(Notice::new(if continuing {
                "cleanup complete"
            } else if queue.candidates.is_empty() {
                "no merge-confirmed sessions are ready"
            } else {
                "select sessions with Space"
            }));
        }
        return Vec::new();
    };
    if let Some(queue) = state.cleanup_queue.as_mut() {
        queue.in_flight = Some(session);
        queue.feedback = None;
    }
    vec![Effect::RemoveSession {
        workspace: state.workspace,
        session,
        force: false,
        force_delete_branch: false,
        purge_orphan: false,
    }]
}

fn update_cleanup_queue(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(queue) = state.cleanup_queue.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.cleanup_queue = None;
        }
        AppKey::Up if !queue.candidates.is_empty() => {
            queue.cursor = queue
                .cursor
                .checked_sub(1)
                .unwrap_or(queue.candidates.len() - 1);
        }
        AppKey::Down if !queue.candidates.is_empty() => {
            queue.cursor = (queue.cursor + 1) % queue.candidates.len();
        }
        AppKey::Char(' ') if queue.in_flight.is_none() => {
            if let Some(session) = queue.candidates.get(queue.cursor).copied() {
                if !queue.selected.insert(session) {
                    queue.selected.remove(&session);
                }
                queue.feedback = None;
            }
        }
        AppKey::Char('a' | 'A') if queue.in_flight.is_none() => {
            if queue.selected.len() == queue.candidates.len() {
                queue.selected.clear();
            } else {
                queue.selected = queue.candidates.iter().copied().collect();
            }
            queue.feedback = None;
        }
        AppKey::Enter => return begin_next_cleanup(state, false),
        _ => {}
    }
    Vec::new()
}

fn continue_cleanup_after_snapshot(state: &mut AppState) -> Vec<Effect> {
    let completed = state
        .cleanup_queue
        .as_ref()
        .and_then(CleanupQueueState::in_flight)
        .filter(|session| !state.sessions.contains(session));
    let Some(completed) = completed else {
        reconcile_cleanup_queue(state);
        return Vec::new();
    };
    if let Some(queue) = state.cleanup_queue.as_mut() {
        queue.in_flight = None;
        queue.selected.remove(&completed);
        queue.feedback = None;
    }
    begin_next_cleanup(state, true)
}

fn remove_candidates(state: &AppState) -> Vec<SessionId> {
    let in_flight = state
        .remove_queue
        .as_ref()
        .and_then(RemoveQueueState::in_flight);
    state
        .sessions
        .iter()
        .copied()
        .filter(|session| Some(*session) == in_flight || state.session_can_remove(*session))
        .collect()
}

/// Rebuild explicit selector membership from the latest lifecycle snapshot.
/// Checked stable identities survive only while the same session remains
/// removable; a deleting in-flight target stays visible until completion.
fn reconcile_remove_queue(state: &mut AppState) {
    let candidates = remove_candidates(state);
    let Some(queue) = state.remove_queue.as_mut() else {
        return;
    };
    queue.candidates = candidates;
    queue
        .selected
        .retain(|session| queue.candidates.contains(session));
    queue.cursor = queue.cursor.min(queue.candidates.len().saturating_sub(1));
}

fn begin_next_remove(state: &mut AppState, continuing: bool) -> Vec<Effect> {
    if let Some(queue) = state.remove_queue.as_mut()
        && queue.in_flight.is_some()
    {
        queue.feedback = Some(Notice::new("waiting for the current removal"));
        return Vec::new();
    }
    reconcile_remove_queue(state);
    let next =
        state.remove_queue.as_ref().and_then(|queue| {
            queue.candidates.iter().copied().find(|session| {
                queue.selected.contains(session) && state.session_can_remove(*session)
            })
        });
    let Some(session) = next else {
        if let Some(queue) = state.remove_queue.as_mut() {
            queue.feedback = Some(Notice::new(if continuing {
                "removal complete"
            } else if queue.candidates.is_empty() {
                "no sessions can be removed"
            } else {
                "select sessions with Space"
            }));
        }
        return Vec::new();
    };
    let force = state
        .remove_queue
        .as_ref()
        .is_some_and(RemoveQueueState::force);
    if let Some(queue) = state.remove_queue.as_mut() {
        queue.in_flight = Some(session);
        queue.feedback = None;
    }
    vec![Effect::RemoveSession {
        workspace: state.workspace,
        session,
        force,
        force_delete_branch: force,
        purge_orphan: false,
    }]
}

fn update_remove_queue(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(queue) = state.remove_queue.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.remove_queue = None;
        }
        AppKey::Up | AppKey::Char('k') if !queue.candidates.is_empty() => {
            queue.cursor = queue
                .cursor
                .checked_sub(1)
                .unwrap_or(queue.candidates.len() - 1);
        }
        AppKey::Down | AppKey::Char('j') if !queue.candidates.is_empty() => {
            queue.cursor = (queue.cursor + 1) % queue.candidates.len();
        }
        AppKey::Char(' ') if queue.in_flight.is_none() => {
            if let Some(session) = queue.candidates.get(queue.cursor).copied() {
                if !queue.selected.insert(session) {
                    queue.selected.remove(&session);
                }
                queue.feedback = None;
            }
        }
        AppKey::Enter => return begin_next_remove(state, false),
        _ => {}
    }
    Vec::new()
}

fn continue_remove_after_snapshot(state: &mut AppState) -> Vec<Effect> {
    let completed = state
        .remove_queue
        .as_ref()
        .and_then(RemoveQueueState::in_flight)
        .filter(|session| !state.sessions.contains(session));
    let Some(completed) = completed else {
        reconcile_remove_queue(state);
        return Vec::new();
    };
    if let Some(queue) = state.remove_queue.as_mut() {
        queue.in_flight = None;
        queue.selected.remove(&completed);
        queue.feedback = None;
    }
    begin_next_remove(state, true)
}

fn update_overlay(state: &mut AppState, overlay: Overlay, key: AppKey) -> Vec<Effect> {
    if let Some(effects) = update_overlay_control_chord(state, overlay, &key) {
        return effects;
    }
    // Garden replaces the whole Home frame, so opening it here would discard a
    // command draft or confirmation instead of restoring that front surface on
    // wake. Keep the explicit shortcut subject to the same foreground guard as
    // idle auto-open.
    if matches!(key, AppKey::OpenGarden) {
        return Vec::new();
    }
    if matches!(overlay, Overlay::Closeup) && matches!(key, AppKey::Escape) {
        dismiss_closeup_action_modal(state);
        return Vec::new();
    }
    if overlay == Overlay::CreateSessionError && matches!(key, AppKey::Escape | AppKey::Enter) {
        state.create_session_error = None;
        state.overlay = None;
        return Vec::new();
    }
    if overlay == Overlay::TerminalLaunchError && matches!(key, AppKey::Escape | AppKey::Enter) {
        state.terminal_launch_error = None;
        state.overlay = None;
        return Vec::new();
    }
    if overlay == Overlay::AgentLaunchError && matches!(key, AppKey::Escape | AppKey::Enter) {
        state.agent_launch_error = None;
        state.overlay = None;
        return Vec::new();
    }
    match overlay {
        Overlay::Decisions => update_decisions_overlay(state, key),
        Overlay::CleanupQueue => update_cleanup_queue(state, &key),
        Overlay::RemoveSessions => update_remove_queue(state, &key),
        Overlay::QuitConfirmation => match key {
            // Each answer has its own letter so leaving and quitting are never
            // the same keystroke: `w` returns to Welcome, `q`/`y` end the
            // process, `n`/Esc stay. Enter commits whichever button is focused,
            // and opening the overlay resets focus to Quit, so the historical
            // `Ctrl-Q` + `Enter` still ends the process (#556).
            AppKey::Char('w' | 'W') => commit_exit_choice(state, ExitChoice::Welcome),
            AppKey::Char('q' | 'Q' | 'y' | 'Y') => commit_exit_choice(state, ExitChoice::Quit),
            AppKey::Char('n' | 'N') | AppKey::Escape => commit_exit_choice(state, ExitChoice::Stay),
            AppKey::Enter => commit_exit_choice(state, state.exit_choice),
            AppKey::Right | AppKey::Tab => {
                state.exit_choice = state.exit_choice.shifted(true);
                Vec::new()
            }
            AppKey::Left => {
                state.exit_choice = state.exit_choice.shifted(false);
                Vec::new()
            }
            _ => Vec::new(),
        },
        Overlay::ForceRemoveConfirmation => match key {
            AppKey::Char('y' | 'Y') => commit_force_remove(state, true),
            AppKey::Char('n' | 'N') | AppKey::Escape => commit_force_remove(state, false),
            AppKey::Enter => commit_force_remove(
                state,
                state
                    .force_remove_confirmation
                    .is_some_and(|(_, confirm)| confirm),
            ),
            AppKey::Left | AppKey::Right | AppKey::Tab => {
                if let Some((_, confirm)) = state.force_remove_confirmation.as_mut() {
                    *confirm = !*confirm;
                }
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
        Overlay::Roles => update_role_editor(state, &key),
        Overlay::CreateSession => update_create_session_form(state, &key),
        // Dismissal is handled by the early Enter/Escape/Ctrl-C branch above; any
        // other key is inert while the create-failure dialog owns input.
        Overlay::Prs => update_prs_overlay(state, &key),
        Overlay::Preview => preview::update_preview_overlay(state, &key),
        Overlay::Overview if matches!(key, AppKey::Escape) => {
            state.overlay = None;
            Vec::new()
        }
        Overlay::Daemon => update_daemon_control(state, &key),
        // Presentation resolves list scrolling against the drawn viewport as
        // GardenClick::Scroll. Any key reaching this reducer wakes Home.
        Overlay::Garden => {
            state.overlay = None;
            Vec::new()
        }
        Overlay::CreateSessionError | Overlay::TerminalLaunchError | Overlay::AgentLaunchError => {
            Vec::new()
        }
        Overlay::Overview | Overlay::Closeup => update_management_key(state, key),
    }
}

fn update_daemon_control(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    match key {
        AppKey::Escape => {
            state.overlay = None;
            Vec::new()
        }
        AppKey::Up | AppKey::Left if state.daemon_control.pending.is_none() => {
            state.daemon_control.selected = state.daemon_control.selected.shifted(-1);
            state.daemon_control.result = None;
            Vec::new()
        }
        AppKey::Down | AppKey::Right | AppKey::Tab if state.daemon_control.pending.is_none() => {
            state.daemon_control.selected = state.daemon_control.selected.shifted(1);
            state.daemon_control.result = None;
            Vec::new()
        }
        AppKey::Char('s' | 'S') if state.daemon_control.pending.is_none() => {
            submit_daemon_action(state, DaemonAction::Start)
        }
        AppKey::Char('r' | 'R') if state.daemon_control.pending.is_none() => {
            submit_daemon_action(state, DaemonAction::Restart)
        }
        AppKey::Char('x' | 'X') if state.daemon_control.pending.is_none() => {
            submit_daemon_action(state, DaemonAction::Stop)
        }
        AppKey::Enter if state.daemon_control.pending.is_none() => {
            submit_daemon_action(state, state.daemon_control.selected)
        }
        _ => Vec::new(),
    }
}

fn submit_daemon_action(state: &mut AppState, action: DaemonAction) -> Vec<Effect> {
    let token = PendingToken(state.next_pending_token);
    state.next_pending_token += 1;
    state.daemon_control.selected = action;
    state.daemon_control.pending = Some((action, token));
    state.daemon_control.result = None;
    vec![Effect::DaemonControl {
        workspace: state.workspace,
        action,
        token,
    }]
}

fn update_role_editor(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(editor) = state.role_editor.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.role_editor = None;
            Vec::new()
        }
        AppKey::ToggleRoleScope | AppKey::Tab if !editor.loading && !editor.saving => {
            let scope = match editor.scope {
                RoleEditorScope::Global => RoleEditorScope::Workspace,
                RoleEditorScope::Workspace => RoleEditorScope::Global,
            };
            *editor = RoleEditor::loading(scope);
            vec![Effect::LoadRoles { scope }]
        }
        AppKey::SaveRoles if !editor.loading && !editor.saving => {
            editor.saving = true;
            vec![Effect::SaveRoles {
                scope: editor.scope,
                source: editor.source.clone(),
            }]
        }
        AppKey::Up if !editor.loading && !editor.saving => {
            editor.scroll_up(1);
            Vec::new()
        }
        AppKey::Down if !editor.loading && !editor.saving => {
            editor.scroll_down(1);
            Vec::new()
        }
        AppKey::PageUp if !editor.loading && !editor.saving => {
            editor.scroll_up(ROLE_EDITOR_VIEWPORT_LINES);
            Vec::new()
        }
        AppKey::PageDown if !editor.loading && !editor.saving => {
            editor.scroll_down(ROLE_EDITOR_VIEWPORT_LINES);
            Vec::new()
        }
        AppKey::Enter if !editor.loading && !editor.saving => {
            editor.source.push('\n');
            editor.follow_tail();
            editor.error = None;
            Vec::new()
        }
        AppKey::Backspace if !editor.loading && !editor.saving => {
            editor.source.pop();
            editor.follow_tail();
            editor.error = None;
            Vec::new()
        }
        AppKey::Paste(text) if !editor.loading && !editor.saving => {
            editor.source.push_str(text);
            editor.error = None;
            Vec::new()
        }
        AppKey::Char(character) if !editor.loading && !editor.saving && !character.is_control() => {
            editor.source.push(*character);
            editor.follow_tail();
            editor.error = None;
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Resolve global quit chords at the frontmost-overlay boundary. Returning
/// `Some` means the overlay consumed the key, so route-level detach handling can
/// never observe it. New ordinary overlays inherit the fail-safe swallow branch;
/// only the two explicit Ctrl-C close contracts mutate modal state.
fn update_overlay_control_chord(
    state: &mut AppState,
    overlay: Overlay,
    key: &AppKey,
) -> Option<Vec<Effect>> {
    match key {
        AppKey::CtrlQ => Some(Vec::new()),
        AppKey::CtrlC => {
            match overlay {
                // Close only the action modal and return input to its underlying
                // Closeup, whether it is the base surface or a live pane.
                Overlay::Closeup => {
                    dismiss_closeup_action_modal(state);
                }
                // The create-failure dialog treats Ctrl-C as acknowledgement;
                // route remains untouched beneath the dismissed dialog.
                Overlay::CreateSessionError => {
                    state.create_session_error = None;
                    state.overlay = None;
                }
                Overlay::TerminalLaunchError => {
                    state.terminal_launch_error = None;
                    state.overlay = None;
                }
                Overlay::AgentLaunchError => {
                    state.agent_launch_error = None;
                    state.overlay = None;
                }
                _ => {}
            }
            Some(Vec::new())
        }
        _ => None,
    }
}

fn reconcile_decision_overlay(state: &mut AppState) {
    let Some(overlay) = state.decision_overlay.as_mut() else {
        return;
    };
    if let Some(editor) = &overlay.editor {
        let mut present = false;
        for item in &state.decisions {
            if item.decision_id == editor.decision.decision_id {
                present = true;
                break;
            }
        }
        if !present {
            overlay.editor = None;
        }
    }
    overlay.selected = overlay
        .selected
        .min(state.decisions.len().saturating_sub(1));
}

fn update_decisions_overlay(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    let workspace = state.workspace;
    let Some(overlay) = state.decision_overlay.as_mut() else {
        return Vec::new();
    };
    if overlay.editor.is_some() && matches!(&key, AppKey::Escape) {
        overlay.editor = None;
        return Vec::new();
    }
    if let Some(editor) = overlay.editor.as_mut() {
        return update_decision_editor(workspace, editor, key);
    }
    match key {
        AppKey::Escape => {
            state.overlay = None;
            state.decision_overlay = None;
        }
        AppKey::DecisionPrevious | AppKey::Up => {
            overlay.selected = overlay.selected.saturating_sub(1);
        }
        AppKey::DecisionNext | AppKey::Down => {
            overlay.selected = (overlay.selected + 1).min(state.decisions.len().saturating_sub(1));
        }
        AppKey::Enter => {
            if let Some(decision) = state.decisions.get(overlay.selected).cloned() {
                overlay.editor = Some(DecisionEditor::new(decision));
            }
        }
        _ => {}
    }
    Vec::new()
}

fn update_decision_editor(
    workspace: WorkspaceId,
    editor: &mut DecisionEditor,
    key: AppKey,
) -> Vec<Effect> {
    match key {
        AppKey::DecisionPrevious | AppKey::Up => {
            editor.selected_option = editor.selected_option.saturating_sub(1);
            editor.scroll_offset = None;
            editor.follow_freeform = false;
        }
        AppKey::DecisionNext | AppKey::Down => {
            editor.selected_option =
                (editor.selected_option + 1).min(editor.decision.options.len().saturating_sub(1));
            editor.scroll_offset = None;
            editor.follow_freeform = false;
        }
        AppKey::PageUp => {
            editor.scroll_offset = Some(editor.scroll_offset.unwrap_or_default().saturating_sub(8));
            editor.follow_freeform = false;
        }
        AppKey::PageDown => {
            editor.scroll_offset = Some(editor.scroll_offset.unwrap_or_default().saturating_add(8));
            editor.follow_freeform = false;
        }
        AppKey::SetDecisionFreeform(text) => {
            if editor.decision.allow_freeform {
                editor.freeform = text;
                follow_decision_freeform(editor);
            }
        }
        AppKey::Char(ch) if editor.decision.allow_freeform => {
            editor.freeform.push(ch);
            follow_decision_freeform(editor);
        }
        AppKey::Backspace if editor.decision.allow_freeform => {
            editor.freeform.pop();
            follow_decision_freeform(editor);
        }
        AppKey::Paste(text) if editor.decision.allow_freeform => {
            paste_decision_freeform(editor, &text);
        }
        AppKey::SubmitDecision | AppKey::Enter => {
            let answer = if editor.decision.allow_freeform && !editor.freeform.trim().is_empty() {
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
                workspace,
                decision_id: editor.decision.decision_id,
                answer,
            }];
        }
        _ => {}
    }
    Vec::new()
}

fn follow_decision_freeform(editor: &mut DecisionEditor) {
    editor.scroll_offset = None;
    editor.follow_freeform = true;
    editor.error = None;
}

fn paste_decision_freeform(editor: &mut DecisionEditor, text: &str) {
    editor.freeform.push_str(text);
    follow_decision_freeform(editor);
}

/// Move between usable managed sessions without exposing the synthetic create
/// row or a failed/deleting checkout as a navigation destination. Switch keeps
/// its cursor semantics; Closeup updates the active target and remains Closeup.
fn navigate_session(state: &mut AppState, direction: TabDirection) -> Vec<Effect> {
    if state.overlay.is_some() || state.workspace_drawer_open() {
        return Vec::new();
    }
    let anchor = if matches!(state.route, Route::Home(HomeMode::Closeup)) {
        state.active
    } else {
        match state.selected {
            Selection::Target(Target::Session(session)) => Some(session),
            Selection::Idle | Selection::Target(Target::Root(_)) | Selection::NewSession => {
                state.active
            }
        }
    };
    let candidates = state
        .sessions
        .iter()
        .copied()
        .filter(|session| state.session_can_use(*session))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Vec::new();
    }
    let current = anchor.and_then(|session| {
        candidates
            .iter()
            .position(|candidate| *candidate == session)
    });
    let index = match (current, direction) {
        (Some(index), TabDirection::Next) => (index + 1) % candidates.len(),
        (Some(index), TabDirection::Previous) => (index + candidates.len() - 1) % candidates.len(),
        (None, TabDirection::Next) => 0,
        (None, TabDirection::Previous) => candidates.len() - 1,
    };
    let session = candidates[index];
    state.selected = Selection::Target(Target::Session(session));
    if matches!(state.route, Route::Home(HomeMode::Closeup)) {
        state.active = Some(session);
        state.closeup_action_forced = false;
    }
    Vec::new()
}

/// Open the pending-decision list and ask its owner for a fresh snapshot.
fn open_decisions(state: &mut AppState) -> Vec<Effect> {
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

#[allow(clippy::too_many_lines)] // Exhaustive Home command ownership remains visible in one reducer table.
fn update_management_key(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    match key {
        AppKey::OpenDecisions => open_decisions(state),
        AppKey::Up => {
            state.move_selection(-1);
            Vec::new()
        }
        AppKey::Down => {
            state.move_selection(1);
            Vec::new()
        }
        AppKey::PreviousSession => navigate_session(state, TabDirection::Previous),
        AppKey::NextSession => navigate_session(state, TabDirection::Next),
        AppKey::OpenOverview | AppKey::Char(':') => {
            state.overlay = Some(Overlay::Overview);
            Vec::new()
        }
        AppKey::OpenGarden => {
            state.overlay = Some(Overlay::Garden);
            state.notice = None;
            Vec::new()
        }
        AppKey::OpenCloseupOverlay => {
            if state.active.is_none() {
                return Vec::new();
            }
            state.overlay = Some(Overlay::Closeup);
            state.closeup_action_forced = state.has_live_pane;
            Vec::new()
        }
        AppKey::CtrlA => match state.route {
            Route::Home(HomeMode::Switch) => open_create_session(state),
            Route::Home(HomeMode::Closeup) => {
                if state.active.is_none() {
                    state.route = Route::Home(HomeMode::Switch);
                    return Vec::new();
                }
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
        // Tab cycling belongs to the tab strip, not to a live PTY: a target whose
        // only tabs are interrupted Agent history must still be able to move its
        // selection onto the tab it wants to resume. A live pane is itself a tab,
        // so this is a strict widening of the live-pane condition.
        AppKey::CtrlN
            if state.has_pane_tab && matches!(state.route, Route::Home(HomeMode::Closeup)) =>
        {
            vec![Effect::SelectTab {
                direction: TabDirection::Next,
            }]
        }
        AppKey::CtrlP
            if state.has_pane_tab && matches!(state.route, Route::Home(HomeMode::Closeup)) =>
        {
            vec![Effect::SelectTab {
                direction: TabDirection::Previous,
            }]
        }
        // Ctrl-X force-removes only the cursor's session, discarding a dirty
        // worktree and an unmerged branch. Keep this unavailable while an overlay
        // owns input, and never turn the workspace root, the new-session row, or
        // an ineligible lifecycle into a deletion target.
        AppKey::CtrlX
            if state.overlay.is_none() && matches!(state.route, Route::Home(HomeMode::Switch)) =>
        {
            remove_selected_session(state)
        }
        AppKey::SubmitOverview(input) => submit_overview(state, &input),
        AppKey::SubmitCloseup(input) => submit_closeup(state, &input),
        AppKey::OpenPrs => open_prs(state),
        AppKey::OpenPreview => open_preview(state),
        AppKey::Char('a')
            if matches!(state.route, Route::Home(HomeMode::Closeup)) && !state.has_pane_tab =>
        {
            submit_empty_closeup_shortcut(state, "agent")
        }
        AppKey::Char('t')
            if matches!(state.route, Route::Home(HomeMode::Closeup)) && !state.has_pane_tab =>
        {
            submit_empty_closeup_shortcut(state, "terminal")
        }
        AppKey::Enter
            if matches!(state.route, Route::Home(HomeMode::Closeup)) && !state.has_pane_tab =>
        {
            state.overlay = Some(Overlay::Closeup);
            state.closeup_action_forced = false;
            Vec::new()
        }
        AppKey::Enter | AppKey::Char('t') => activate_selected(state),
        AppKey::CtrlN
        | AppKey::CtrlP
        | AppKey::CtrlX
        | AppKey::Escape
        | AppKey::Tab
        | AppKey::Left
        | AppKey::Right
        | AppKey::Backspace
        | AppKey::Paste(_)
        | AppKey::Home
        | AppKey::Char(_)
        | AppKey::CtrlC
        | AppKey::CtrlQ
        | AppKey::OpenQuitConfirmation
        | AppKey::ToggleDirectorDrawer
        | AppKey::ToggleRootTerminalDrawer
        | AppKey::ToggleRootTerminalFullHeight
        | AppKey::OpenRootTerminal
        | AppKey::OpenDirectorNew
        | AppKey::OpenDirectorOrganization
        | AppKey::OpenDirectorWorkRuns
        | AppKey::OpenDirectorRunOverview(_)
        | AppKey::OpenDirectorConsole(_)
        | AppKey::DirectorBack
        | AppKey::OpenNotes
        | AppKey::OpenEnvironment
        | AppKey::SelectNoteSection(_)
        | AppKey::SetNoteDraft(_)
        | AppKey::CommitNoteDraft
        | AppKey::ToggleTodo(_)
        | AppKey::SaveNotes
        | AppKey::SaveEnvironment
        | AppKey::ToggleRoleScope
        | AppKey::SaveRoles
        | AppKey::DecisionPrevious
        | AppKey::DecisionNext
        | AppKey::PageUp
        | AppKey::PageDown
        | AppKey::SetDecisionFreeform(_)
        | AppKey::SubmitDecision => Vec::new(),
    }
}

fn update_root_terminal_drawer_key(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    match key {
        AppKey::ToggleRootTerminalDrawer => {
            state.root_terminal_drawer_open = false;
            state.root_terminal_full_height = false;
            state.workspace_drawer_focus = state
                .director_drawer_open
                .then_some(WorkspaceDrawerFocus::Director);
            Vec::new()
        }
        AppKey::ToggleRootTerminalFullHeight => {
            state.root_terminal_full_height = !state.root_terminal_full_height;
            Vec::new()
        }
        AppKey::ToggleDirectorDrawer => {
            state.root_terminal_full_height = false;
            state.director_drawer_open = true;
            state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Director);
            state.director_new = DirectorNew::Idle;
            Vec::new()
        }
        AppKey::OpenDirectorNew => {
            state.root_terminal_full_height = false;
            state.director_drawer_open = true;
            state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Director);
            open_director_new(state);
            Vec::new()
        }
        AppKey::OpenDirectorWorkRuns => {
            if state.work_mode != WorkMode::GoalDriven
                || state.director_launching.is_some()
                || !matches!(state.director_new, DirectorNew::Idle)
            {
                return Vec::new();
            }
            state.root_terminal_full_height = false;
            state.director_drawer_open = true;
            state.workspace_drawer_focus = Some(WorkspaceDrawerFocus::Director);
            state.director_route = DirectorRoute::WorkRuns;
            state.director_new = DirectorNew::Idle;
            state.director_goal.clear();
            Vec::new()
        }
        AppKey::OpenRootTerminal => vec![Effect::OpenTerminal {
            target: Target::Root(state.workspace),
            operation_id: OperationId::new(),
            arguments: "new".to_owned(),
        }],
        // The root shell owns ordinary input, including Escape. All other Home
        // mutations remain inert while the drawer is frontmost.
        AppKey::CtrlN
        | AppKey::CtrlP
        | AppKey::PreviousSession
        | AppKey::NextSession
        | AppKey::Escape
        | AppKey::Tab
        | AppKey::Left
        | AppKey::Right
        | AppKey::Backspace
        | AppKey::Paste(_)
        | AppKey::Home
        | AppKey::Char(_)
        | AppKey::CtrlA
        | AppKey::CtrlO
        | AppKey::CtrlC
        | AppKey::CtrlQ
        | AppKey::CtrlX
        | AppKey::OpenQuitConfirmation
        | AppKey::OpenOverview
        | AppKey::OpenDirectorOrganization
        | AppKey::OpenDirectorRunOverview(_)
        | AppKey::OpenDirectorConsole(_)
        | AppKey::DirectorBack
        | AppKey::OpenCloseupOverlay
        | AppKey::OpenNotes
        | AppKey::OpenEnvironment
        | AppKey::OpenGarden
        | AppKey::OpenPrs
        | AppKey::OpenPreview
        | AppKey::OpenDecisions
        | AppKey::DecisionPrevious
        | AppKey::DecisionNext
        | AppKey::PageUp
        | AppKey::PageDown
        | AppKey::SetDecisionFreeform(_)
        | AppKey::SubmitDecision
        | AppKey::SelectNoteSection(_)
        | AppKey::SetNoteDraft(_)
        | AppKey::CommitNoteDraft
        | AppKey::ToggleTodo(_)
        | AppKey::SaveNotes
        | AppKey::SaveEnvironment
        | AppKey::ToggleRoleScope
        | AppKey::SaveRoles
        | AppKey::SubmitOverview(_)
        | AppKey::SubmitCloseup(_)
        | AppKey::Enter
        | AppKey::Up
        | AppKey::Down => Vec::new(),
    }
}

/// Request removal for Switch's selected session and keep the cursor on that
/// stable identity while the presentation turns the row into a loading skeleton.
/// Snapshot reconciliation moves it only after the daemon removes the row.
///
/// Ctrl-X is a force removal: it discards an uncommitted worktree and an
/// unmerged branch for the exact selected identity. The safe variant it
/// replaced refused nearly every real session — Git rejects `worktree remove`
/// while the tree carries untracked build output, and rejects `branch -d`
/// unless the branch is merged into the local base or the daemon can prove a
/// squash merge from its PR inventory — so the usual outcome was a
/// `failed/delete` row that then needed a second, forced attempt anyway.
///
/// `purge_orphan` stays reserved for a daemon-diagnosed integrity orphan: the
/// daemon rejects that acknowledgement for any other row.
fn remove_selected_session(state: &AppState) -> Vec<Effect> {
    let Selection::Target(Target::Session(session)) = state.selected else {
        return Vec::new();
    };
    let integrity_orphan = state
        .session_lifecycles
        .get(&session)
        .is_some_and(|projection| {
            projection.lifecycle == SessionLifecycle::Failed
                && projection.failure_stage == Some(FailureStage::Integrity)
        });
    if !state.sessions.contains(&session) {
        return Vec::new();
    }
    if !integrity_orphan && !state.session_can_remove(session) {
        return Vec::new();
    }
    vec![Effect::RemoveSession {
        workspace: state.workspace,
        session,
        force: true,
        force_delete_branch: true,
        purge_orphan: integrity_orphan,
    }]
}

fn update_editor_key(state: &mut AppState, key: &AppKey) -> Option<Vec<Effect>> {
    let notes_open = state.overlay == Some(Overlay::Notes);
    let environment_open = state.overlay == Some(Overlay::Environment);
    if let Some(effects) = update_environment_source_key(state, key, environment_open) {
        return Some(effects);
    }
    match key {
        AppKey::OpenNotes => Some(open_notes(state)),
        AppKey::OpenEnvironment => Some(open_environment_source(state, EnvScope::Workspace)),
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
        AppKey::Paste(text) if notes_open => Some(paste_note_draft(state, text)),
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
        _ => None,
    }
}

fn update_environment_source_key(
    state: &mut AppState,
    key: &AppKey,
    environment_open: bool,
) -> Option<Vec<Effect>> {
    if !environment_source_is_open(state, environment_open) {
        return None;
    }
    match key {
        AppKey::Tab => {
            if let Some(editor) = editable_environment(state, environment_open) {
                if editor.scope == EnvScope::Global {
                    return Some(Vec::new());
                }
                editor
                    .source
                    .toggle_save_focus(editor.scope != EnvScope::Global);
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::Enter => Some(enter_environment_source(state, environment_open)),
        AppKey::Char(character) => {
            if let Some(editor) = editable_environment(state, environment_open) {
                editor.source.insert(&character.to_string());
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::Backspace => {
            if let Some(editor) = editable_environment(state, environment_open) {
                editor.source.backspace();
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::Left | AppKey::Right => {
            if let Some(editor) = editable_environment(state, environment_open) {
                editor.source.move_cursor(matches!(key, AppKey::Right));
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::Up | AppKey::Down => {
            if let Some(editor) = editable_environment(state, environment_open) {
                editor.source.move_vertical(matches!(key, AppKey::Down));
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::Paste(text) => {
            if let Some(editor) = editable_environment(state, environment_open) {
                editor.source.paste(text);
                editor.error = None;
            }
            Some(Vec::new())
        }
        AppKey::SaveEnvironment | AppKey::SaveRoles => {
            Some(save_environment_source(state, environment_open))
        }
        _ => None,
    }
}

fn environment_source_is_open(state: &AppState, environment_open: bool) -> bool {
    environment_open && state.environment_editor.as_ref().is_some()
}

fn enter_environment_source(state: &mut AppState, environment_open: bool) -> Vec<Effect> {
    let save_focused = state
        .environment_editor
        .as_ref()
        .is_some_and(EnvironmentEditor::is_save_focused);
    if save_focused {
        save_environment_source(state, environment_open)
    } else {
        if let Some(editor) = editable_environment(state, environment_open) {
            editor.source.newline();
            editor.error = None;
        }
        Vec::new()
    }
}

fn save_environment_source(state: &mut AppState, environment_open: bool) -> Vec<Effect> {
    let Some(editor) = editable_environment(state, environment_open) else {
        return Vec::new();
    };
    let bindings = match editor.source.parse() {
        Ok(bindings) => bindings,
        Err(message) => {
            editor.error = Some(SafeError {
                message: SafeMessage::new(message),
                error_id: "environment-invalid-source".to_owned(),
            });
            editor.source.focus_source();
            return Vec::new();
        }
    };
    editor.entries = bindings
        .into_iter()
        .map(|(name, value)| EnvironmentEntry { name, value })
        .collect();
    editor.saving = true;
    vec![Effect::SaveEnvironment {
        scope: editor.scope,
        entries: editor.entries.clone(),
    }]
}

fn paste_note_draft(state: &mut AppState, text: &str) -> Vec<Effect> {
    if let Some(editor) = state.note_editor.as_mut() {
        editor.draft.push_str(text);
        editor.error = None;
    }
    Vec::new()
}

/// The environment editor when it owns input and accepts edits (no read or save
/// in flight).
fn editable_environment(
    state: &mut AppState,
    environment_open: bool,
) -> Option<&mut EnvironmentEditor> {
    state
        .environment_editor
        .as_mut()
        .filter(|editor| environment_open && !editor.is_busy())
}

fn open_notes(state: &mut AppState) -> Vec<Effect> {
    let Some(target) = state.active_target() else {
        return Vec::new();
    };
    state.overlay = Some(Overlay::Notes);
    state.environment_editor = None;
    state.note_editor = Some(NoteEditor::loading(target));
    vec![Effect::LoadNotes { target }]
}

/// The scope named by the `env` command's argument. No argument edits this
/// workspace's own bindings — the common case — and `global` edits the bindings
/// every workspace inherits. Anything else is refused rather than guessed.
fn environment_scope(arguments: &str) -> Option<EnvScope> {
    match arguments.trim() {
        "" | "workspace" => Some(EnvScope::Workspace),
        "global" => Some(EnvScope::Global),
        _ => None,
    }
}

/// Open the Config-style environment source editor pinned to `scope`.
fn open_environment_source(state: &mut AppState, scope: EnvScope) -> Vec<Effect> {
    state.overlay = Some(Overlay::Environment);
    state.note_editor = None;
    state.environment_editor = Some(EnvironmentEditor::loading(scope));
    vec![Effect::LoadEnvironment { scope }]
}

fn open_prs(state: &mut AppState) -> Vec<Effect> {
    let Some(target) = state.active_target() else {
        return Vec::new();
    };
    open_prs_for_target(state, target)
}

fn open_prs_for_target(state: &mut AppState, target: Target) -> Vec<Effect> {
    let mut overlay = PrOverlay::loading(target);
    if let Target::Session(session) = target
        && let Some(prs) = state.session_prs(session)
    {
        overlay.prs = filtered_prs(prs, PrFilter::All);
    }
    // Keep the request state so a newly returned PR can still open immediately,
    // but do not render an empty loading/empty-state modal.
    state.overlay = (!overlay.prs.is_empty()).then_some(Overlay::Prs);
    state.pr_overlay = Some(overlay);
    state.preview_overlay = None;
    vec![Effect::LoadPullRequests { target }]
}

fn open_preview(state: &mut AppState) -> Vec<Effect> {
    let Some(target) = state.file_preview_target() else {
        return Vec::new();
    };
    state.overlay = Some(Overlay::Preview);
    let overlay = PreviewOverlay::loading(target);
    let request_id = overlay.request_id();
    state.preview_overlay = Some(overlay);
    state.pr_overlay = None;
    vec![Effect::LoadPreview {
        target,
        request_id,
        path: None,
        filter: PreviewFileFilter::All,
    }]
}

/// Pull Request overlay の入力を還元する。←→ で status tab、↑↓ で PR 選択を回し、
/// Enter で選択 PR を browser で開く effect を出す。Esc は overlay を閉じる。
/// 素材の再取得はしない。
fn update_prs_overlay(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    if matches!(key, AppKey::Left | AppKey::Right) {
        let Some((target, filter)) = state.pr_overlay.as_ref().map(|overlay| {
            let filter = if matches!(key, AppKey::Right) {
                overlay.filter.next()
            } else {
                overlay.filter.previous()
            };
            (overlay.target, filter)
        }) else {
            return Vec::new();
        };
        let all = target
            .session_id()
            .and_then(|session| state.session_prs(session))
            .unwrap_or_default()
            .to_vec();
        if let Some(overlay) = state.pr_overlay.as_mut() {
            overlay.filter = filter;
            overlay.prs = filtered_prs(&all, filter);
            overlay.selected = 0;
        }
        return Vec::new();
    }
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
                url: pr.url().to_owned(),
            })
            .into_iter()
            .collect(),
        AppKey::Char('c') => overlay
            .selected_pr()
            .map(|pr| Effect::CopyPullRequest {
                url: pr.url().to_owned(),
            })
            .into_iter()
            .collect(),
        AppKey::CtrlX => overlay
            .selected_pr()
            .and_then(|pr| {
                overlay
                    .target
                    .session_id()
                    .map(|session| Effect::DismissPullRequest {
                        session,
                        url: pr.url().to_owned(),
                    })
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

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
        NoteSection::Note => {
            editor.scratchpad.note = if draft.is_empty() {
                None
            } else {
                Some(draft.to_owned())
            };
        }
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

fn submit_overview(state: &mut AppState, input: &str) -> Vec<Effect> {
    if state.overlay != Some(Overlay::Overview) {
        return Vec::new();
    }
    match overview::interpret(input) {
        Ok(overview::Command::Config { arguments }) => {
            if arguments.trim().is_empty() {
                state.overlay = None;
                state.notice = None;
                vec![Effect::WorkspaceCommand {
                    workspace: state.workspace,
                    command: overview::Command::Config { arguments },
                }]
            } else {
                state.notice = Some(Notice::new("config takes no arguments (usage: config)"));
                Vec::new()
            }
        }
        Ok(overview::Command::Daemon { arguments }) => {
            if arguments.trim().is_empty() {
                state.overlay = Some(Overlay::Daemon);
                state.daemon_control = DaemonControlState::default();
                state.notice = None;
            } else {
                state.notice = Some(Notice::new("daemon takes no arguments (usage: daemon)"));
            }
            Vec::new()
        }
        Ok(overview::Command::Clean { arguments }) => {
            if overview::parse_clean(&arguments).is_err() {
                state.notice = Some(Notice::new(
                    "invalid clean arguments (usage: clean [--apply [--force]])",
                ));
                Vec::new()
            } else {
                state.overlay = None;
                state.notice = Some(Notice::new("Inspecting orphan session resources"));
                vec![Effect::WorkspaceCommand {
                    workspace: state.workspace,
                    command: overview::Command::Clean { arguments },
                }]
            }
        }
        Ok(overview::Command::Garden { arguments }) => {
            if arguments.trim().is_empty() {
                if state.garden_available {
                    state.overlay = Some(Overlay::Garden);
                    state.notice = None;
                } else {
                    state.overlay = None;
                    state.notice = Some(Notice::new(
                        "Garden needs a terminal at least 64 columns wide and 14 rows tall",
                    ));
                }
            } else {
                state.notice = Some(Notice::new("garden takes no arguments (usage: garden)"));
            }
            Vec::new()
        }
        Ok(overview::Command::Env { arguments }) => {
            if let Some(scope) = environment_scope(&arguments) {
                open_environment_source(state, scope)
            } else {
                state.notice = Some(Notice::new(
                    "env takes an optional scope (usage: env [workspace|global])",
                ));
                Vec::new()
            }
        }
        Ok(overview::Command::Roles { arguments }) => {
            let scope = match arguments.trim() {
                "" | "workspace" => Some(RoleEditorScope::Workspace),
                "global" => Some(RoleEditorScope::Global),
                _ => None,
            };
            if let Some(scope) = scope {
                state.overlay = Some(Overlay::Roles);
                state.role_editor = Some(RoleEditor::loading(scope));
                vec![Effect::LoadRoles { scope }]
            } else {
                state.notice = Some(Notice::new(
                    "roles takes an optional scope (usage: roles [workspace|global])",
                ));
                Vec::new()
            }
        }
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

fn submit_overview_session(state: &mut AppState, arguments: &str) -> Vec<Effect> {
    let command = match overview::parse_session(arguments) {
        Ok(command) => command,
        Err(message) => {
            state.notice = Some(Notice::new(message));
            return Vec::new();
        }
    };
    match command {
        overview::SessionCommand::Create {
            name,
            role_id,
            base_ref,
        } => {
            state.overlay = None;
            request_create_session(
                state,
                SessionCreateIntent {
                    name,
                    base_ref,
                    profile: None,
                    model: None,
                    role_id,
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
        overview::SessionCommand::Cleanup => {
            let candidates = cleanup_candidates(state);
            state.overlay = Some(Overlay::CleanupQueue);
            state.remove_queue = None;
            state.cleanup_queue = Some(CleanupQueueState::new(candidates));
            state.notice = None;
            vec![Effect::SyncPullRequestTargets {
                sessions: state.sessions.clone(),
            }]
        }
        overview::SessionCommand::Resume { name } => {
            state.overlay = None;
            let Some(index) = state
                .session_names
                .iter()
                .position(|candidate| candidate == &name)
            else {
                state.notice = Some(Notice::new("session was not found"));
                return Vec::new();
            };
            let session = state.sessions[index];
            state.notice = Some(Notice::new("Resuming provider conversation"));
            vec![Effect::ResumeAgent {
                workspace: state.workspace,
                session,
                operation_id: OperationId::new(),
            }]
        }
        overview::SessionCommand::Sleep { name } => {
            state.overlay = None;
            let Some(index) = state
                .session_names
                .iter()
                .position(|candidate| candidate == &name)
            else {
                state.notice = Some(Notice::new("session was not found"));
                return Vec::new();
            };
            let session = state.sessions[index];
            state.notice = Some(Notice::new("Putting idle Agent to sleep"));
            vec![Effect::SleepSession {
                workspace: state.workspace,
                session,
            }]
        }
        overview::SessionCommand::SelectRemove { force } => open_remove_selector(state, force),
        overview::SessionCommand::Remove {
            name,
            force,
            force_delete_branch,
            purge_orphan,
        } => remove_named_session(state, &name, force, force_delete_branch, purge_orphan),
    }
}

fn open_remove_selector(state: &mut AppState, force: bool) -> Vec<Effect> {
    let candidates = remove_candidates(state);
    let cursor = match state.selected {
        Selection::Target(Target::Session(selected)) => candidates
            .iter()
            .position(|candidate| *candidate == selected)
            .unwrap_or_default(),
        Selection::Idle | Selection::Target(Target::Root(_)) | Selection::NewSession => 0,
    };
    state.overlay = Some(Overlay::RemoveSessions);
    state.cleanup_queue = None;
    state.remove_queue = Some(RemoveQueueState::new(candidates, cursor, force));
    state.notice = None;
    Vec::new()
}

fn remove_named_session(
    state: &mut AppState,
    name: &str,
    force: bool,
    force_delete_branch: bool,
    purge_orphan: bool,
) -> Vec<Effect> {
    let session = state
        .session_names
        .iter()
        .position(|candidate| candidate == name)
        .and_then(|index| state.sessions.get(index).copied());
    let Some(session) = session else {
        state.notice = Some(Notice::new("session was not found"));
        return Vec::new();
    };
    if !state.session_can_remove(session) {
        state.notice = Some(Notice::new("session cannot be removed"));
        return Vec::new();
    }
    state.overlay = None;
    state.remove_queue = None;
    state.notice = Some(Notice::new("Removing session"));
    vec![Effect::RemoveSession {
        workspace: state.workspace,
        session,
        force,
        force_delete_branch,
        purge_orphan,
    }]
}

fn submit_closeup(state: &mut AppState, input: &str) -> Vec<Effect> {
    if state.overlay != Some(Overlay::Closeup) {
        return Vec::new();
    }
    let Some(active_session) = state
        .active
        .filter(|session| state.sessions.contains(session) && state.session_can_use(*session))
    else {
        // Managed-session Closeup has no workspace-root fallback. A stale modal
        // is dismissed without emitting terminal/Agent/diff effects.
        state.overlay = None;
        state.route = Route::Home(HomeMode::Switch);
        state.closeup_action_forced = false;
        return Vec::new();
    };
    let active_target = Target::Session(active_session);
    let command = match closeup::interpret(input) {
        Ok(command) => command,
        Err(error) => {
            state.notice = Some(Notice::new(error.to_string()));
            return Vec::new();
        }
    };
    let command_name = command.name();
    // Which CLI an accepted `agent` resolved to, so the confirmation names the
    // selection (including when it came from the configured default).
    let mut selection = None;
    let effect = match command {
        closeup::Command::Terminal { arguments } => match terminal_arguments(&arguments) {
            Ok(arguments) if arguments == "new" => Some(Effect::OpenExternalTerminal {
                target: active_target,
            }),
            Ok(arguments) => Some(Effect::OpenTerminal {
                target: active_target,
                operation_id: OperationId::new(),
                arguments,
            }),
            Err(error) => {
                state.notice = Some(error);
                None
            }
        },
        // `agent [-m <cli>]` selects one installed CLI; an omitted `-m` uses the
        // configured default. A workspace-root Agent (`Target::Root`) runs in
        // the trusted repository root; a session Agent runs in that session's
        // worktree. The daemon resolves the checkout path in both cases.
        closeup::Command::Agent { arguments } => {
            match agent_command::parse(&arguments, state.default_model, state.available_models) {
                Ok(request) => {
                    selection = Some(if request.from_default {
                        format!("{} (default)", request.model.selector())
                    } else {
                        request.model.selector().to_owned()
                    });
                    Some(Effect::LaunchAgent {
                        workspace: state.workspace,
                        session: Some(active_session),
                        operation_id: OperationId::new(),
                        profile: Some(profile_for(request.model)),
                    })
                }
                Err(error) => {
                    state.notice = Some(Notice::new(error));
                    None
                }
            }
        }
        closeup::Command::Close { arguments } => {
            if let Some(force) = parse_close_force(&arguments) {
                // One meaning of "force" across every TUI removal: `-f` drops a
                // dirty worktree *and* an unmerged branch, exactly like Switch's
                // `Ctrl-X`.
                Some(Effect::RemoveSession {
                    workspace: state.workspace,
                    session: active_session,
                    force,
                    force_delete_branch: force,
                    purge_orphan: false,
                })
            } else {
                state.notice = Some(Notice::new("invalid close arguments"));
                None
            }
        }
        closeup::Command::Diff { .. } => {
            state.notice = Some(Notice::new(format!("{command_name} is not available")));
            None
        }
        // `env` owns the workspace-scoped editor rather than a per-session effect,
        // so it opens the editor and returns before the shared dismiss/notice tail.
        closeup::Command::Env { arguments } => return submit_closeup_env(state, &arguments),
        closeup::Command::Workflow { arguments } => {
            return submit_closeup_workflow(state, active_session, &arguments);
        }
    };
    if effect.is_some() {
        dismiss_closeup_action_modal(state);
        state.notice = Some(Notice::new(match selection {
            Some(selection) => format!("Requested {command_name} {selection}"),
            None => format!("Requested {command_name}"),
        }));
    }
    effect.into_iter().collect()
}

/// Dispatch one primary action from an empty Closeup without making the action
/// modal its default surface. The existing command boundary remains the single
/// owner of validation, notices, and effect construction.
fn submit_empty_closeup_shortcut(state: &mut AppState, input: &str) -> Vec<Effect> {
    state.overlay = Some(Overlay::Closeup);
    state.closeup_action_forced = false;
    submit_closeup(state, input)
}

fn submit_closeup_workflow(
    state: &mut AppState,
    session: SessionId,
    arguments: &str,
) -> Vec<Effect> {
    if !arguments.is_empty() {
        state.notice = Some(Notice::new("workflow takes no arguments"));
        return Vec::new();
    }
    let panel = state.workflows.entry(session).or_default();
    let load = !panel.loading && !panel.submitting;
    panel.loading |= load;
    dismiss_closeup_action_modal(state);
    let mut effects = vec![Effect::OpenWorkflow { session }];
    if load {
        effects.push(Effect::Workflow(super::workflow::WorkflowJob {
            workspace: state.workspace,
            session,
            control: None,
        }));
    }
    effects
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

fn parse_close_force(arguments: &str) -> Option<bool> {
    crate::usecase::session_remove::parse(arguments)
        .ok()
        .filter(|request| request.target.is_none())
        .map(|request| request.force)
}

/// Open the environment editor from Closeup, reusing the Overview `env` grammar.
/// The editor is workspace-scoped, so a Closeup launch edits the same bindings as
/// Overview rather than any session-specific environment.
fn submit_closeup_env(state: &mut AppState, arguments: &str) -> Vec<Effect> {
    if arguments.trim().is_empty() {
        open_environment_source(state, EnvScope::Workspace)
    } else {
        state.notice = Some(Notice::new("env takes no arguments (usage: env)"));
        Vec::new()
    }
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
    if state.overlay.is_some() || state.workspace_drawer_open() {
        state.pending_session_click = None;
        return Vec::new();
    }
    if let Some(session) = state.sidebar_pr_at(column, row) {
        state.pending_session_click = None;
        return open_prs_for_target(state, Target::Session(session));
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

/// Open the Garden once Home has been idle for [`GARDEN_IDLE_THRESHOLD`].
///
/// The duration is injected (see [`AppEvent::IdleElapsed`]), so this reducer
/// stays a pure comparison and needs no clock to test.
fn update_idle(state: &mut AppState, elapsed: std::time::Duration) -> Vec<Effect> {
    if elapsed >= GARDEN_IDLE_THRESHOLD && garden_may_auto_open(state) {
        state.overlay = Some(Overlay::Garden);
    }
    Vec::new()
}

/// Whether an idle Home may be covered by the screen saver.
///
/// Any open overlay keeps the garden away: that is what protects a confirmation
/// dialog, an unsent form draft, and the read-only surfaces alike, without this
/// predicate having to enumerate them. The Director drawer is excluded for the
/// same reason. Everything left — ordinary Switch, and a Closeup with no overlay
/// in front of it, live terminal included — is eligible, and the daemon-owned
/// processes behind it keep running.
///
/// Terminal size is *not* checked here. The minimum the garden needs is a
/// renderer layout fact owned by presentation, which suppresses the event
/// entirely on a terminal too small to draw a garden (`presentation::views::
/// workspace::garden_fits`).
fn garden_may_auto_open(state: &AppState) -> bool {
    state.garden_available && state.overlay.is_none() && !state.workspace_drawer_open()
}

/// Reduce a click the presentation layer already resolved against the garden's
/// own hitboxes.
///
/// List scrolling keeps the Garden open. Other clicks close it; a target also
/// activates its session. A session that
/// disappeared from the snapshot between the frame and the press is a stale
/// target, so it closes the garden and does nothing else.
fn update_garden_click(state: &mut AppState, click: GardenClick) -> Vec<Effect> {
    if state.overlay != Some(Overlay::Garden) {
        return Vec::new();
    }
    if let GardenClick::Scroll { offset } = click {
        state.garden_sidebar_scroll = offset;
        return Vec::new();
    }
    state.overlay = None;
    state.garden_sidebar_scroll = 0;
    let GardenClick::Visit {
        workspace, session, ..
    } = click
    else {
        return Vec::new();
    };
    if workspace != state.workspace {
        return Vec::new();
    }
    visit_session(state, session)
}

fn visit_session(state: &mut AppState, session: SessionId) -> Vec<Effect> {
    focus_session(state, session);
    let selection = Selection::Target(Target::Session(session));
    if state.selected != selection {
        return Vec::new();
    }
    // The same activation the sidebar performs, including its refusal to attach
    // an unusable checkout. The garden adds no target semantics of its own.
    activate_selected(state)
}

fn focus_session(state: &mut AppState, session: SessionId) -> Vec<Effect> {
    state.select_row(Selection::Target(Target::Session(session)));
    Vec::new()
}

fn activate_selected(state: &mut AppState) -> Vec<Effect> {
    match state.selected {
        Selection::Target(Target::Session(session))
            if state
                .session_lifecycles
                .get(&session)
                .is_some_and(|projection| {
                    projection.lifecycle == SessionLifecycle::Failed
                        && projection.failure_stage == Some(FailureStage::Delete)
                }) =>
        {
            state.force_remove_confirmation = Some((session, true));
            state.overlay = Some(Overlay::ForceRemoveConfirmation);
            Vec::new()
        }
        // A Failed row is not a usable checkout (`can_use=false`): it stays
        // selected so it can be removed, but activation does not attach it or open
        // its Closeup terminal surface. Usable targets and the workspace root are
        // unaffected.
        Selection::Target(Target::Session(session)) if !state.session_can_use(session) => {
            Vec::new()
        }
        Selection::Target(Target::Session(session)) if state.sessions.contains(&session) => {
            state.active = Some(session);
            state.route = Route::Home(HomeMode::Closeup);
            state.closeup_action_forced = false;
            state.overlay = None;
            Vec::new()
        }
        // Root and stale session selections are not Home rows and cannot open a
        // managed Closeup.
        Selection::Idle | Selection::Target(Target::Root(_) | Target::Session(_)) => Vec::new(),
        Selection::NewSession => open_create_session(state),
    }
}

fn open_create_session(state: &mut AppState) -> Vec<Effect> {
    // Ctrl-A opens this persistent sidebar action directly, so keep the visual
    // cursor and the inline form on the same `+ new session` row. The active
    // target remains unchanged.
    state.selected = Selection::NewSession;
    state.create_session = Some(CreateSessionForm::with_catalogs(
        state.session_names.clone(),
        &state.role_catalog,
        &state.branch_catalog,
    ));
    state.overlay = Some(Overlay::CreateSession);
    Vec::new()
}

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
        AppKey::Paste(text) => {
            form.paste(text);
            Vec::new()
        }
        AppKey::Up => {
            form.move_branch(true);
            Vec::new()
        }
        AppKey::Down => {
            form.move_branch(false);
            Vec::new()
        }
        AppKey::Tab => {
            form.move_role(false);
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

#[cfg(test)]
mod tests;

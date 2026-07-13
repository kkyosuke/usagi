//! Home の純粋な application controller。
//!
//! [`update`] は TUI-local の [`AppEvent`] を受け、状態を更新して外部へ依頼する
//! [`Effect`] を返す。daemon の wire 型はここへ持ち込まない。実行側は
//! [`BackendPort`] で effect を backend 固有の command に変換し、テストでは
//! [`FakeBackend`] の command log と event queue を使う。

use std::collections::VecDeque;
use std::path::PathBuf;

use usagi_core::domain::agent::{AgentProfileId, ModelSelector};
use usagi_core::domain::id::{AgentRuntimeRef, OperationId, SessionId, WorkspaceId};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::session_lifecycle::AgentPhase;

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
}

/// 新規 session 入力で編集する項目。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CreateSessionField {
    #[default]
    Name,
    Profile,
    Model,
}

/// daemon へ送る前の、TUI-local な新規 session 入力。
///
/// profile/model は空なら未指定であり、daemon の workspace default policy に委ねる。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CreateSessionForm {
    name: String,
    profile: String,
    model: String,
    field: CreateSessionField,
    error: Option<Notice>,
}

impl CreateSessionForm {
    #[must_use]
    pub const fn field(&self) -> CreateSessionField {
        self.field
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
    #[must_use]
    pub fn error(&self) -> Option<&Notice> {
        self.error.as_ref()
    }

    fn selected_mut(&mut self) -> &mut String {
        match self.field {
            CreateSessionField::Name => &mut self.name,
            CreateSessionField::Profile => &mut self.profile,
            CreateSessionField::Model => &mut self.model,
        }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            CreateSessionField::Name => CreateSessionField::Profile,
            CreateSessionField::Profile => CreateSessionField::Model,
            CreateSessionField::Model => CreateSessionField::Name,
        };
    }

    fn push(&mut self, character: char) {
        self.selected_mut().push(character);
        self.error = None;
    }

    fn backspace(&mut self) {
        self.selected_mut().pop();
        self.error = None;
    }

    fn request(&mut self) -> Result<SessionCreateIntent, Notice> {
        let name = required_create_value(&self.name, "session name is required")?;
        let profile = optional_profile(&self.profile)?;
        let model = optional_model(&self.model)?;
        Ok(SessionCreateIntent {
            name,
            profile,
            model,
        })
    }
}

/// Validated new-session request. This is intentionally product-neutral: adapter
/// specific CLI flags and model allowlists remain daemon adapter concerns.
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

fn optional_model(value: &str) -> Result<Option<ModelSelector>, Notice> {
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        ModelSelector::new(value)
            .map(Some)
            .map_err(|_| Notice::new("invalid model selector"))
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    route: Route,
    overlay: Option<Overlay>,
    note_editor: Option<NoteEditor>,
    environment_editor: Option<EnvironmentEditor>,
    create_session: Option<CreateSessionForm>,
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
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
    has_live_pane: bool,
    closeup_action_forced: bool,
    ctrl_c_grace: bool,
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
            create_session: None,
            workspace,
            sessions,
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
            has_live_pane: false,
            closeup_action_forced: false,
            ctrl_c_grace: false,
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
    /// Open environment editor, including unsaved values after a save failure.
    #[must_use]
    #[coverage(off)]
    pub fn environment_editor(&self) -> Option<&EnvironmentEditor> {
        self.environment_editor.as_ref()
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
        let Target::Session(session) = target else {
            return TargetPhase::Absent;
        };
        self.runtimes
            .iter()
            .filter(|entry| entry.runtime.session_id == session)
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
    /// Whether the next management `Ctrl-C` is deliberately absorbed.
    #[must_use]
    #[coverage(off)]
    pub const fn ctrl_c_grace(&self) -> bool {
        self.ctrl_c_grace
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

/// terminal adapter が将来投影する入力語彙。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppKey {
    /// cursor を前の row へ動かす。
    Up,
    /// cursor を次の row へ動かす。
    Down,
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
    /// Management-screen Ctrl-C. Live Ctrl-C is classified before it reaches
    /// this reducer and is passed through to the PTY.
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
        KeyCode::Char(character) if !key.modifiers.control => Some(AppKey::Char(character)),
        _ => None,
    }
}

/// reducer の入力。実 terminal adapter はこの語彙へ変換するだけでよい。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// live terminal input。現行 Home reducer は接続 seam を提供し、pane routing は runtime 合成側が担う。
    Input(LiveInput),
    /// A runtime pane became available or left the projection. A transition
    /// from live to non-live arms the one-shot Ctrl-C grace reducer.
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
    /// target の terminal を開くか再利用する。
    OpenTerminal {
        target: Target,
        /// Durable identity used to make a repeated reducer delivery harmless.
        operation_id: OperationId,
        /// Normalized terminal UX mode: `open` or `new`.
        arguments: String,
    },
    /// Start an Agent through the daemon for an existing session.  The
    /// operation ID is generated by the TUI and survives acceptance/replay.
    LaunchAgent {
        workspace: WorkspaceId,
        session: SessionId,
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
pub fn update(state: &mut AppState, event: AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Backend(event) if update_editor_backend(state, &event) => Vec::new(),
        AppEvent::Key(key) => update_key(state, key),
        AppEvent::LivePaneAvailability(has_live_pane) => {
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
            | BackendEvent::EnvironmentError { .. },
        ) => Vec::new(),
        AppEvent::Backend(BackendEvent::Sessions(sessions)) => {
            state.sessions = sessions;
            state
                .runtimes
                .retain(|entry| state.sessions.contains(&entry.runtime.session_id));
            state.reconcile_selection();
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Notice(notice)) => {
            state.notice = Some(notice);
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::RuntimePhase { runtime, phase }) => {
            if runtime.terminal.workspace_id != state.workspace
                || runtime.terminal.session_id != Some(runtime.session_id)
                || !state.sessions.contains(&runtime.session_id)
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
            state.notice = result.notice;
            if result.succeeded
                && let (Some(pending), Some(created)) = (pending, result.created)
                && pending.interaction_at_accept == state.interaction_count
            {
                state.sessions.push(created);
                state.selected = Selection::Target(Target::Session(created));
                state.active = Target::Session(created);
                state.route = Route::Home(HomeMode::Closeup);
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
            if std::mem::take(&mut state.ctrl_c_grace) {
                state.notice = Some(Notice::new("Ctrl-C ignored after leaving live pane"));
                Vec::new()
            } else if state.has_live_pane {
                state.overlay = Some(Overlay::QuitConfirmation);
                Vec::new()
            } else {
                vec![Effect::Detach]
            }
        }
        AppKey::CtrlQ | AppKey::OpenQuitConfirmation => {
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
    if matches!(key, AppKey::CtrlC | AppKey::CtrlQ) {
        return Vec::new();
    }
    match overlay {
        Overlay::QuitConfirmation => match key {
            AppKey::Char('y' | 'Y') | AppKey::Enter => {
                state.overlay = None;
                vec![Effect::Detach]
            }
            AppKey::Char('n' | 'N') | AppKey::Escape => {
                state.overlay = None;
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
        Overlay::Overview if matches!(key, AppKey::Escape) => {
            state.overlay = None;
            Vec::new()
        }
        Overlay::Closeup if matches!(key, AppKey::Escape) => {
            state.closeup_action_forced = false;
            state.overlay = (!state.has_live_pane).then_some(Overlay::Closeup);
            Vec::new()
        }
        Overlay::Overview | Overlay::Closeup => update_management_key(state, key),
    }
}

#[coverage(off)]
fn update_management_key(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    match key {
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
        AppKey::SubmitOverview(input) => submit_overview(state, &input),
        AppKey::SubmitCloseup(input) => submit_closeup(state, &input),
        AppKey::Enter | AppKey::Char('t') => activate_selected(state),
        AppKey::CtrlN
        | AppKey::CtrlP
        | AppKey::Escape
        | AppKey::Tab
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
        | AppKey::SaveEnvironment => Vec::new(),
    }
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
        overview::SessionCommand::Remove { force } => match state.active {
            Target::Session(session) => {
                state.overlay = None;
                vec![Effect::RemoveSession {
                    workspace: state.workspace,
                    session,
                    force,
                }]
            }
            Target::Root(_) => {
                state.notice = Some(Notice::new("workspace root cannot be removed"));
                Vec::new()
            }
        },
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
        closeup::Command::Agent { arguments } => match state.active {
            Target::Session(session) => match optional_profile(&arguments) {
                Ok(profile) => Some(Effect::LaunchAgent {
                    workspace: state.workspace,
                    session,
                    operation_id: OperationId::new(),
                    profile,
                }),
                Err(error) => {
                    state.notice = Some(error);
                    None
                }
            },
            Target::Root(_) => {
                state.notice = Some(Notice::new("workspace root cannot start an agent"));
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
    match arguments {
        "" => Some(false),
        "--force" => Some(true),
        _ => None,
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
    state.create_session = Some(CreateSessionForm::default());
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
        AppKey::Tab => {
            form.next_field();
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
        // Ctrl-A/Home and unsupported keys must never retrigger create while
        // this form owns input.
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

    #[test]
    fn create_session_form_edits_and_validates_each_field() {
        let mut form = CreateSessionForm::default();
        assert_eq!(form.field(), CreateSessionField::Name);
        assert_eq!(form.name(), "");
        assert_eq!(form.profile(), "");
        assert_eq!(form.model(), "");
        assert!(form.error().is_none());

        assert!(required_create_value(" ", "required").is_err());
        assert_eq!(required_create_value(" name ", "required").unwrap(), "name");
        assert_eq!(optional_profile("").unwrap(), None);
        assert!(optional_profile("invalid profile").is_err());
        assert_eq!(optional_model("").unwrap(), None);
        assert!(optional_model("invalid\nmodel").is_err());

        for character in "session".chars() {
            form.push(character);
        }
        form.next_field();
        for character in "codex".chars() {
            form.push(character);
        }
        form.next_field();
        for character in "gpt-5".chars() {
            form.push(character);
        }
        form.backspace();
        form.push('5');

        let request = form.request().unwrap();
        assert_eq!(request.name, "session");
        assert_eq!(request.profile.unwrap().as_str(), "codex");
        assert_eq!(request.model.unwrap().as_str(), "gpt-5");
        form.next_field();
        assert_eq!(form.field(), CreateSessionField::Name);
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
            session,
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
                name: "closeup overlay returns to closeup origin",
                events: vec![
                    AppEvent::LivePaneAvailability(true),
                    AppEvent::Key(AppKey::Enter),
                    AppEvent::Key(AppKey::OpenCloseupOverlay),
                    AppEvent::Key(AppKey::Escape),
                ],
                route: Route::Home(HomeMode::Closeup),
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
    fn management_ctrl_c_detaches_without_live_pane_and_confirms_with_one() {
        let (workspace, session, _) = ids();
        let mut idle = AppState::home(workspace, Vec::new());
        assert_eq!(
            update(&mut idle, AppEvent::Key(AppKey::CtrlC)),
            vec![Effect::Detach]
        );

        let mut live = AppState::home(workspace, vec![session]);
        let _ = update(&mut live, AppEvent::LivePaneAvailability(true));
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
        assert!(update(&mut state, AppEvent::Key(AppKey::Char('n'))).is_empty());
        assert_eq!(state.overlay(), None);

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenQuitConfirmation));
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::Enter)),
            vec![Effect::Detach]
        );
    }

    #[test]
    fn leaving_live_pane_arms_one_shot_ctrl_c_grace_and_other_input_clears_it() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
        assert!(state.ctrl_c_grace());
        assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
        assert!(!state.ctrl_c_grace());
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::CtrlC)),
            vec![Effect::Detach]
        );

        let _ = update(&mut state, AppEvent::LivePaneAvailability(true));
        let _ = update(&mut state, AppEvent::LivePaneAvailability(false));
        assert!(state.ctrl_c_grace());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert!(!state.ctrl_c_grace());
        assert_eq!(
            update(&mut state, AppEvent::Key(AppKey::CtrlC)),
            vec![Effect::Detach]
        );
    }

    #[test]
    fn ordinary_modals_keep_ctrl_c_and_ctrl_q_inert() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        for overlay_key in [AppKey::OpenOverview, AppKey::OpenCloseupOverlay] {
            let _ = update(&mut state, AppEvent::Key(overlay_key));
            let expected = state.overlay();
            assert!(update(&mut state, AppEvent::Key(AppKey::CtrlC)).is_empty());
            assert_eq!(state.overlay(), expected);
            assert!(update(&mut state, AppEvent::Key(AppKey::CtrlQ)).is_empty());
            assert_eq!(state.overlay(), expected);
            let _ = update(&mut state, AppEvent::Key(AppKey::Escape));
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
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        assert_eq!(
            state.create_session_form().unwrap().field(),
            CreateSessionField::Name
        );
        let _ = update(&mut state, AppEvent::Key(AppKey::Home));
        assert_eq!(state.overlay(), Some(Overlay::CreateSession));
        for key in [
            AppKey::Char('w'),
            AppKey::Char('o'),
            AppKey::Char('r'),
            AppKey::Char('k'),
            AppKey::Tab,
            AppKey::Char('c'),
            AppKey::Char('o'),
            AppKey::Char('d'),
            AppKey::Char('e'),
            AppKey::Char('x'),
            AppKey::Tab,
            AppKey::Char('g'),
            AppKey::Char('p'),
            AppKey::Char('t'),
            AppKey::Char('-'),
            AppKey::Char('5'),
        ] {
            let _ = update(&mut state, AppEvent::Key(key));
        }
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert!(matches!(
            &effects[..],
            [Effect::CreateSession { workspace: actual_workspace, token: PendingToken(1), intent, .. }]
                if *actual_workspace == workspace
                    && intent.name == "work"
                    && intent.profile.as_ref().is_some_and(|profile| profile.as_str() == "codex")
                    && intent.model.as_ref().is_some_and(|model| model.as_str() == "gpt-5")
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
        let _ = update(&mut state, AppEvent::Key(AppKey::Escape));

        // Ctrl-O is the Closeup-to-Switch pane-navigation transition.
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
                AppEvent::Key(AppKey::SubmitOverview("session remove --force".into())),
            ),
            vec![Effect::RemoveSession {
                workspace,
                session,
                force: true,
            }]
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
            }] if *effect_workspace == workspace && *effect_session == session && profile.as_str() == "codex"
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
}

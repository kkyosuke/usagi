//! Home の純粋な application controller。
//!
//! [`update`] は TUI-local の [`AppEvent`] を受け、状態を更新して外部へ依頼する
//! [`Effect`] を返す。daemon の wire 型はここへ持ち込まない。実行側は
//! [`BackendPort`] で effect を backend 固有の command に変換し、テストでは
//! [`FakeBackend`] の command log と event queue を使う。

use std::collections::VecDeque;

use usagi_core::domain::id::{AgentRuntimeRef, SessionId, WorkspaceId};
use usagi_core::domain::session_lifecycle::AgentPhase;

use crate::usecase::terminal_input::{LiveInput, RuntimeEvent};
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
    const fn rank(self) -> u8 {
        match self {
            Self::Absent => 0,
            Self::Ready => 1,
            Self::Running => 2,
            Self::Waiting => 3,
            Self::Done => 4,
        }
    }

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
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
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
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    selected: Selection,
    active: Target,
    notice: Option<Notice>,
    runtimes: Vec<RuntimePhase>,
    feedback: Option<Feedback>,
    pending: Vec<PendingOperation>,
    next_pending_token: u64,
    size: Option<(u16, u16)>,
}

impl AppState {
    /// workspace root を selected / active にした Home を作る。
    #[must_use]
    pub fn home(workspace: WorkspaceId, sessions: Vec<SessionId>) -> Self {
        let root = Target::Root(workspace);
        Self {
            route: Route::Home(HomeMode::Switch),
            overlay: None,
            workspace,
            sessions,
            selected: Selection::Target(root),
            active: root,
            notice: None,
            runtimes: Vec::new(),
            feedback: None,
            pending: Vec::new(),
            next_pending_token: 1,
            size: None,
        }
    }

    /// 常駐 route。
    #[must_use]
    pub const fn route(&self) -> Route {
        self.route
    }
    /// 最前面 overlay。閉じても [`route`](Self::route) は変わらない。
    #[must_use]
    pub const fn overlay(&self) -> Option<Overlay> {
        self.overlay
    }
    /// navigation cursor。
    #[must_use]
    pub const fn selected(&self) -> Selection {
        self.selected
    }
    /// command / Closeup の target。
    #[must_use]
    pub const fn active(&self) -> Target {
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
    pub const fn size(&self) -> Option<(u16, u16)> {
        self.size
    }

    fn root(&self) -> Target {
        Target::Root(self.workspace)
    }

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
    /// overlay を閉じるか Closeup から Switch へ戻る。
    Escape,
    /// workspace scope overlay を開く。
    OpenOverview,
    /// target scope overlay を開く。
    OpenCloseupOverlay,
    /// 将来の terminal input / command vocabulary 用の文字入力。
    Char(char),
    /// Overview modal の現在の入力を registry 経由で実行する。
    SubmitOverview(String),
    /// Closeup modal の現在の入力を registry 経由で実行する。
    SubmitCloseup(String),
}

/// reducer の入力。実 terminal adapter はこの語彙へ変換するだけでよい。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// live terminal input。現行 Home reducer は接続 seam を提供し、pane routing は runtime 合成側が担う。
    Input(LiveInput),
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
}

/// 非同期 request の成否。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationResult {
    /// 完了した request の token。
    pub token: PendingToken,
    /// 成功したか。
    pub succeeded: bool,
    /// 画面へ表示してよい補足。失敗時は safe message だけを渡す。
    pub notice: Option<Notice>,
}

/// reducer が要求する外部操作。daemon wire 型への変換は adapter 側の責務。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// session create を backend に依頼する。
    CreateSession {
        workspace: WorkspaceId,
        token: PendingToken,
    },
    /// 次の snapshot を要求する。
    RefreshSessions { workspace: WorkspaceId },
    /// workspace scope command を backend adapter に依頼する。
    WorkspaceCommand {
        workspace: WorkspaceId,
        command: overview::Command,
    },
    /// target の terminal を開くか再利用する。
    OpenTerminal { target: Target, arguments: String },
    /// target の agent を開くか再利用する。
    OpenAgent { target: Target, arguments: String },
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
    pub const fn route(&self) -> &EntryRoute {
        &self.route
    }

    /// Registered Open Single choices.
    #[must_use]
    pub fn workspaces(&self) -> &[EntryWorkspace] {
        &self.workspaces
    }

    /// Recent typed identities displayed by Welcome.
    #[must_use]
    pub fn recents(&self) -> &[WorkspaceId] {
        &self.recents
    }

    /// The attach currently in flight, if any.
    #[must_use]
    pub const fn opening(&self) -> Option<WorkspaceId> {
        self.opening
    }

    /// The last attach error, suitable for rendering on the current entry screen.
    #[must_use]
    pub fn error(&self) -> Option<&Notice> {
        self.error.as_ref()
    }

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
    pub fn push_event(&mut self, event: EntryEvent) {
        self.events.push_back(event);
    }

    /// Effects dispatched by the entry reducer.
    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
}

/// Dispatch entry effects and replay queued fake-backend completions.
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
    pub fn push_event(&mut self, event: BackendEvent) {
        self.events.push_back(event);
    }
    /// dispatch された effect を確認する。
    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
    /// effect log を取り出し、空にする。
    #[must_use]
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

impl BackendPort for FakeBackend {
    fn dispatch(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
    fn next_event(&mut self) -> Option<BackendEvent> {
        self.events.pop_front()
    }
}

/// event を state へ還元し、必要な外部 effect を返す。
#[must_use]
pub fn update(state: &mut AppState, event: AppEvent) -> Vec<Effect> {
    match event {
        AppEvent::Key(key) => update_key(state, key),
        AppEvent::Resize { width, height } => {
            state.size = Some((width, height));
            Vec::new()
        }
        AppEvent::Input(_) | AppEvent::Tick => Vec::new(),
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
            state
                .pending
                .retain(|pending| pending.token != result.token);
            state.notice = result.notice;
            if result.succeeded {
                vec![Effect::RefreshSessions {
                    workspace: state.workspace,
                }]
            } else {
                Vec::new()
            }
        }
    }
}

fn update_key(state: &mut AppState, key: AppKey) -> Vec<Effect> {
    if matches!(key, AppKey::Escape) && state.overlay.take().is_some() {
        return Vec::new();
    }
    match key {
        AppKey::Up => {
            state.move_selection(-1);
            Vec::new()
        }
        AppKey::Down => {
            state.move_selection(1);
            Vec::new()
        }
        AppKey::Escape => match state.route {
            Route::Home(HomeMode::Switch) => Vec::new(),
            Route::Home(HomeMode::Closeup) => {
                state.route = Route::Home(HomeMode::Switch);
                Vec::new()
            }
        },
        AppKey::OpenOverview | AppKey::Char(':') => {
            state.overlay = Some(Overlay::Overview);
            Vec::new()
        }
        AppKey::OpenCloseupOverlay => {
            state.overlay = Some(Overlay::Closeup);
            Vec::new()
        }
        AppKey::SubmitOverview(input) => submit_overview(state, &input),
        AppKey::SubmitCloseup(input) => submit_closeup(state, &input),
        AppKey::Enter | AppKey::Char('t') => activate_selected(state),
        AppKey::Char(_) => Vec::new(),
    }
}

fn submit_overview(state: &mut AppState, input: &str) -> Vec<Effect> {
    if state.overlay != Some(Overlay::Overview) {
        return Vec::new();
    }
    match overview::interpret(input) {
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
        closeup::Command::Terminal { arguments } => Some(Effect::OpenTerminal {
            target: state.active,
            arguments,
        }),
        closeup::Command::Agent { arguments } => Some(Effect::OpenAgent {
            target: state.active,
            arguments,
        }),
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
        closeup::Command::Chat { .. } | closeup::Command::Diff { .. } => {
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

fn parse_close_force(arguments: &str) -> Option<bool> {
    match arguments {
        "" => Some(false),
        "--force" => Some(true),
        _ => None,
    }
}

fn activate_selected(state: &mut AppState) -> Vec<Effect> {
    match state.selected {
        Selection::Target(target) => {
            state.active = target;
            state.route = Route::Home(HomeMode::Closeup);
            Vec::new()
        }
        Selection::NewSession => {
            let token = PendingToken(state.next_pending_token);
            state.next_pending_token += 1;
            state.pending.push(PendingOperation {
                token,
                kind: PendingKind::CreateSession,
            });
            vec![Effect::CreateSession {
                workspace: state.workspace,
                token,
            }]
        }
    }
}

/// effect を dispatch し、queue 済みの backend event を reducer へ戻すテスト helper。
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
    use usagi_core::domain::id::{
        AgentRuntimeId, DaemonGeneration, TerminalId, TerminalRef, WorktreeId,
    };

    fn ids() -> (WorkspaceId, SessionId, SessionId) {
        (WorkspaceId::new(), SessionId::new(), SessionId::new())
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
                    AppEvent::Key(AppKey::Enter),
                    AppEvent::Key(AppKey::OpenCloseupOverlay),
                    AppEvent::Key(AppKey::Escape),
                ],
                route: Route::Home(HomeMode::Closeup),
                overlay: None,
            },
            Case {
                name: "closeup escape returns switch",
                events: vec![AppEvent::Key(AppKey::Enter), AppEvent::Key(AppKey::Escape)],
                route: Route::Home(HomeMode::Switch),
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
    fn new_session_is_selectable_but_never_active_and_requests_backend_once() {
        let (workspace, _, _) = ids();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        assert_eq!(state.selected(), Selection::NewSession);
        let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
        assert_eq!(state.active(), Target::Root(workspace));
        assert_eq!(
            effects,
            vec![Effect::CreateSession {
                workspace,
                token: PendingToken(1)
            }]
        );
        assert_eq!(
            state.pending(),
            &[PendingOperation {
                token: PendingToken(1),
                kind: PendingKind::CreateSession
            }]
        );
        assert_eq!(state.pending()[0].token.get(), 1);

        let effects = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token: PendingToken(1),
                succeeded: true,
                notice: Some(Notice::new("created")),
            }),
        );
        assert!(state.pending().is_empty());
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("created")
        );
        assert_eq!(effects, vec![Effect::RefreshSessions { workspace }]);

        let effects = update(
            &mut state,
            AppEvent::OperationResult(OperationResult {
                token: PendingToken(99),
                succeeded: false,
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
        assert_eq!(
            effects,
            vec![Effect::OpenTerminal {
                target: Target::Session(session),
                arguments: "open".to_owned(),
            }]
        );
        assert!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitCloseup("terminal open".to_owned())),
            )
            .is_empty()
        );
    }

    #[test]
    fn closeup_registry_dispatches_agent_and_validated_session_remove() {
        let (workspace, session, _) = ids();
        let mut state = AppState::home(workspace, vec![session]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Down));
        let _ = update(&mut state, AppEvent::Key(AppKey::Enter));

        let _ = update(&mut state, AppEvent::Key(AppKey::OpenCloseupOverlay));
        assert_eq!(
            update(
                &mut state,
                AppEvent::Key(AppKey::SubmitCloseup("agent codex".to_owned())),
            ),
            vec![Effect::OpenAgent {
                target: Target::Session(session),
                arguments: "codex".to_owned(),
            }]
        );

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
    }

    #[test]
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
}

//! Home の純粋な application controller。
//!
//! [`update`] は TUI-local の [`AppEvent`] を受け、状態を更新して外部へ依頼する
//! [`Effect`] を返す。daemon の wire 型はここへ持ち込まない。実行側は
//! [`BackendPort`] で effect を backend 固有の command に変換し、テストでは
//! [`FakeBackend`] の command log と event queue を使う。

use std::collections::VecDeque;

use usagi_core::domain::id::{SessionId, WorkspaceId};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// reducer の入力。実 terminal adapter はこの語彙へ変換するだけでよい。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
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

/// backend が TUI-local projection として返す event。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    /// stable identity で表した session snapshot。
    Sessions(Vec<SessionId>),
    /// backend が safe と保証した notice。
    Notice(Notice),
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
        AppEvent::Tick => Vec::new(),
        AppEvent::Backend(BackendEvent::Sessions(sessions)) => {
            state.sessions = sessions;
            state.reconcile_selection();
            Vec::new()
        }
        AppEvent::Backend(BackendEvent::Notice(notice)) => {
            state.notice = Some(notice);
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
        AppKey::Enter | AppKey::Char('t') => activate_selected(state),
        AppKey::Char(_) => Vec::new(),
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

    fn ids() -> (WorkspaceId, SessionId, SessionId) {
        (WorkspaceId::new(), SessionId::new(), SessionId::new())
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
    fn navigation_wraps_up_and_ignores_non_command_characters() {
        let (workspace, first, _) = ids();
        let mut state = AppState::home(workspace, vec![first]);
        let _ = update(&mut state, AppEvent::Key(AppKey::Up));
        assert_eq!(state.selected(), Selection::NewSession);
        let _ = update(&mut state, AppEvent::Key(AppKey::Char('x')));
        assert_eq!(state.selected(), Selection::NewSession);
    }
}

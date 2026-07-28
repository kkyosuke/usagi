//! Closeup の terminal / Agent tab を扱う純粋な reducer。
//!
//! daemon の inventory や stream はここに持ち込まない。adapter は request と completion、
//! exit を [`PaneEvent`] に翻訳し、[`reduce`] が返す [`PaneEffect`] だけを実行する。
//! tab の identity は表示名ではなく、完全な [`TerminalRef`] である。

use usagi_core::domain::id::{AgentContinuationRef, OperationId, TerminalRef};

use super::controller::{TabDirection, Target};
use super::interrupted_tab::InterruptedTab;

/// Closeup tab が表示する terminal 種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    /// shell などの通常 terminal。
    Terminal,
    /// terminal 上で起動する Agent。
    Agent,
    /// A read-only diff document for the selected target.
    Diff,
}

/// backend の completion を待つ tab placeholder。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingPane {
    /// terminal reservation を要求した durable operation。
    pub operation: OperationId,
    /// request を発行した Closeup target。
    pub target: Target,
    /// resolving / starting 中の pane 種別。
    pub kind: PaneKind,
}

/// attach できる live tab。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePane {
    /// 端末の incarnation を含む stable identity。
    pub terminal: TerminalRef,
    /// tab に表示する terminal 種別。
    pub kind: PaneKind,
}

/// One interrupted Agent conversation shown as its own tab (#510).
///
/// The tab is read-only: it owns no terminal incarnation, so it never attaches,
/// resizes, or accepts input. Only the user's explicit Resume action fills
/// [`Self::resuming`], and only a validated replacement turns the tab live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedPane {
    /// Projected, non-sensitive conversation. Its lineage is the tab identity.
    pub tab: InterruptedTab,
    /// The explicit resume operation while it is in flight. `None` leaves the
    /// tab inert, and a second activation converges to this same operation.
    pub resuming: Option<OperationId>,
}

/// Closeup tab。pending は operation、live は terminal incarnation で識別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTab {
    /// terminal resolve または Agent start の完了待ち。
    Pending(PendingPane),
    /// attach 済みまたは attach 可能な terminal。
    Live(LivePane),
    /// A non-terminal pane whose request completed successfully.
    ///
    /// Diff has no daemon terminal incarnation to attach, but it still uses
    /// the same request/pending/completion tab lifecycle as terminal and
    /// Agent.  Its operation remains the stable tab identity.
    Ready(PendingPane),
    /// An interrupted Agent conversation awaiting an explicit resume (#510).
    Interrupted(InterruptedPane),
}

/// tab を index でなく stable identity により選ぶための key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabSelection {
    /// completion 前の placeholder。
    Pending(OperationId),
    /// completion 後または保存済みの live tab。
    Live(TerminalRef),
    /// A completed non-terminal tab.
    Ready(OperationId),
    /// An interrupted Agent conversation, keyed by its durable lineage so the
    /// selection survives inventory refresh and its own resume.
    Interrupted(AgentContinuationRef),
}

/// attach 判断に必要な TUI-local の選択位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneSelection {
    /// No managed target is active (session 0 Home).
    None,
    /// Closeup target が選択中。これに対する request は completion 時に attach する。
    Target(Target),
    /// tab が選択中。pending placeholder の completion だけが attach 対象になる。
    Tab(TabSelection),
}

/// Closeup pane の reducer state。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneState {
    tabs: Vec<PaneTab>,
    selected: PaneSelection,
    error: Option<String>,
}

impl PaneState {
    /// target または tab の現在の選択で空の pane state を作る。
    #[must_use]
    pub fn new(selected: PaneSelection) -> Self {
        Self {
            tabs: Vec::new(),
            selected,
            error: None,
        }
    }

    /// local storage から復元した live tabs で state を作る。
    #[must_use]
    pub fn with_live(selected: PaneSelection, tabs: Vec<LivePane>) -> Self {
        Self {
            tabs: tabs.into_iter().map(PaneTab::Live).collect(),
            selected,
            error: None,
        }
    }

    /// tab の表示順。
    #[must_use]
    pub fn tabs(&self) -> &[PaneTab] {
        &self.tabs
    }

    /// 現在の target / tab 選択。
    #[must_use]
    pub fn selected(&self) -> &PaneSelection {
        &self.selected
    }

    /// 直近の safe pane error。
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// tab を一つでも所有しているか。
    #[must_use]
    pub fn has_tabs(&self) -> bool {
        !self.tabs.is_empty()
    }
}

/// Closeup tab state の target-scoped registry。
///
/// target を切り替えても entry を破棄しないため、session ごとの pending、selected
/// tab、explicit action modal state は互いに混ざらない。表示中でない target の
/// completion や exit も、その entry だけを還元する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRegistry {
    /// The managed target whose pane is shown. `None` when Home has no active
    /// managed session; the registry never falls back to `Target::Root`.
    active: Option<Target>,
    entries: Vec<PaneRegistryEntry>,
    inactive: PaneState,
    revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneRegistryEntry {
    target: Target,
    pane: PaneState,
    action_modal_forced: bool,
}

/// Closeup の現在の input owner。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneInputOwner {
    /// action modal が management input を所有する。
    ActionModal,
    /// selected tab が terminal input を所有する。
    Tab,
}

/// registry へ dispatch する target-scoped event。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneRegistryEvent {
    /// 表示 target を切り替える。既存 target の tab state は保持する。
    SelectTarget(Target),
    /// active managed target を外す。既存 entry は background state として保持する。
    ClearTarget,
    /// `target` が所有する pane reducer だけへ event を渡す。
    Pane { target: Target, event: PaneEvent },
    /// tab があっても action modal を明示的に開く。
    OpenActionModal { target: Target },
    /// forced modal を閉じる。tab が無い target の modal は閉じない。
    CloseActionModal { target: Target },
}

/// tab owner にだけ届く Closeup command。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTabCommand {
    /// stable tab identity で選択を変更する。
    Select(TabSelection),
    /// selected tab を client-side から外す。
    Close,
    /// Move the selected stable tab within this target's display order.
    Reorder(TabDirection),
    /// terminal input を runtime へ渡す。
    Passthrough,
}

/// registry reducer が返す effect。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneRegistryEffect {
    /// target-scoped pane effect。terminal lifecycle の所有権は移さない。
    Pane { target: Target, effect: PaneEffect },
    /// tab owner が terminal runtime へ入力を渡す。
    Passthrough { target: Target },
}

impl PaneRegistry {
    /// `active` target を持つ空の registry を作る。`None` は active managed target
    /// を持たない Home（session 0 件）で、root を fallback にしない。
    #[must_use]
    pub fn new(active: impl Into<Option<Target>>) -> Self {
        let active = active.into();
        let entries = active
            .map(|target| vec![PaneRegistryEntry::empty(target)])
            .unwrap_or_default();
        Self {
            active,
            entries,
            inactive: PaneState::new(PaneSelection::None),
            revision: 0,
        }
    }

    /// 現在表示する target。active managed target が無ければ `None`。
    #[must_use]
    pub const fn active(&self) -> Option<Target> {
        self.active
    }

    /// Monotonic fence for every effective registry mutation. Restore jobs
    /// capture it at dispatch so a delayed snapshot cannot reorder or refocus a
    /// registry the user has changed meanwhile.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// `target` 固有の pane state。未訪問 target は空 state として投影する。
    #[must_use]
    pub fn pane(&self, target: Target) -> Option<&PaneState> {
        self.entries
            .iter()
            .find(|entry| entry.target == target)
            .map(|entry| &entry.pane)
    }

    /// 現在表示中 target の pane state。active managed target が無ければ、tab を
    /// 持たない inert state を返す。
    #[must_use]
    pub fn active_pane(&self) -> &PaneState {
        self.active
            .and_then(|active| {
                self.entries
                    .iter()
                    .find(|entry| entry.target == active)
                    .map(|entry| &entry.pane)
            })
            .unwrap_or(&self.inactive)
    }

    /// 全 target の live tab を表示順に列挙する。
    ///
    /// selected foreground 以外は detach 済みなので stream を持たない
    /// ([3. TUI](../../../../document/03-tui.md))。shell はこの一覧から
    /// background 分を切り出し、scope inventory による exit 観測へ渡す。
    #[must_use]
    pub fn live_terminals(&self) -> Vec<TerminalRef> {
        self.entries
            .iter()
            .flat_map(|entry| entry.pane.tabs())
            .filter_map(|tab| match tab {
                PaneTab::Live(live) => Some(live.terminal.clone()),
                PaneTab::Pending(_) | PaneTab::Ready(_) | PaneTab::Interrupted(_) => None,
            })
            .collect()
    }

    /// action modal の表示 predicate。空 pane は常に modal が所有する。
    #[must_use]
    pub fn action_modal_visible(&self, target: Target) -> bool {
        self.entries
            .iter()
            .find(|entry| entry.target == target)
            .is_none_or(|entry| !entry.pane.has_tabs() || entry.action_modal_forced)
    }

    /// active target の input owner。
    #[must_use]
    pub fn input_owner(&self) -> PaneInputOwner {
        // Without an active managed target there is no tab to own input, so the
        // action-modal owner is reported (an empty Home never routes tab input).
        let owned_by_tab = self
            .active
            .is_some_and(|active| !self.action_modal_visible(active));
        if owned_by_tab {
            PaneInputOwner::Tab
        } else {
            PaneInputOwner::ActionModal
        }
    }

    fn entry_mut(&mut self, target: Target) -> &mut PaneRegistryEntry {
        if let Some(index) = self.entries.iter().position(|entry| entry.target == target) {
            return &mut self.entries[index];
        }
        self.entries.push(PaneRegistryEntry::empty(target));
        self.entries
            .last_mut()
            .expect("pushing a registry entry leaves one entry")
    }
}

impl PaneRegistryEntry {
    fn empty(target: Target) -> Self {
        Self {
            target,
            pane: PaneState::new(PaneSelection::Target(target)),
            action_modal_forced: false,
        }
    }
}

/// `event` を一つの target entry へ還元する。
#[must_use]
pub fn reduce_registry(
    registry: &mut PaneRegistry,
    event: PaneRegistryEvent,
) -> Vec<PaneRegistryEffect> {
    let before_active = registry.active;
    let before_entries = registry.entries.clone();
    let effects = match event {
        PaneRegistryEvent::SelectTarget(target) => {
            registry.active = Some(target);
            registry.entry_mut(target);
            Vec::new()
        }
        PaneRegistryEvent::ClearTarget => {
            registry.active = None;
            Vec::new()
        }
        PaneRegistryEvent::OpenActionModal { target } => {
            registry.entry_mut(target).action_modal_forced = true;
            Vec::new()
        }
        PaneRegistryEvent::CloseActionModal { target } => {
            let entry = registry.entry_mut(target);
            if entry.pane.has_tabs() {
                entry.action_modal_forced = false;
            }
            Vec::new()
        }
        PaneRegistryEvent::Pane { target, event } => {
            if !event_belongs_to_target(&event, target) {
                return Vec::new();
            }
            let active = registry.active == Some(target);
            let entry = registry.entry_mut(target);
            if matches!(event, PaneEvent::Request { .. }) {
                entry.action_modal_forced = false;
            }
            reduce(&mut entry.pane, event)
                .into_iter()
                // A background target keeps its own state, but cannot attach a
                // stream or change the visible Closeup projection.
                .filter(|_| active)
                .map(|effect| PaneRegistryEffect::Pane { target, effect })
                .collect()
        }
    };
    if registry.active != before_active || registry.entries != before_entries {
        registry.revision = registry.revision.saturating_add(1);
    }
    effects
}

/// Route a tab command only when the active target's tab owns input.
///
/// # Panics
///
/// Panics if the registry reports a tab input owner without an active target,
/// which the [`PaneRegistry::input_owner`] contract makes unreachable.
#[must_use]
pub fn route_tab_command(
    registry: &mut PaneRegistry,
    command: PaneTabCommand,
) -> Vec<PaneRegistryEffect> {
    if registry.input_owner() != PaneInputOwner::Tab {
        return Vec::new();
    }
    let target = registry
        .active
        .expect("tab input owner always has an active target");
    match command {
        PaneTabCommand::Select(selection) => reduce_registry(
            registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Select(PaneSelection::Tab(selection)),
            },
        ),
        PaneTabCommand::Close => reduce_registry(
            registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::CloseSelected,
            },
        ),
        PaneTabCommand::Reorder(direction) => reduce_registry(
            registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::ReorderSelected(direction),
            },
        ),
        PaneTabCommand::Passthrough => vec![PaneRegistryEffect::Passthrough { target }],
    }
}

fn event_belongs_to_target(event: &PaneEvent, target: Target) -> bool {
    match event {
        PaneEvent::Select(PaneSelection::None) => false,
        PaneEvent::Select(PaneSelection::Target(selected)) => *selected == target,
        // Tab-keyed events name their own stable identity, so only the entry
        // that owns that tab reacts. Resume progress is keyed by lineage the
        // same way.
        PaneEvent::Select(PaneSelection::Tab(_))
        | PaneEvent::Succeeded { .. }
        | PaneEvent::Resolved { .. }
        | PaneEvent::Failed { .. }
        | PaneEvent::ReorderSelected(_)
        | PaneEvent::CloseSelected
        | PaneEvent::ResumeStarted { .. }
        | PaneEvent::ResumeReplaced { .. }
        | PaneEvent::ResumeFailed { .. } => true,
        PaneEvent::Request {
            target: requested, ..
        } => *requested == target,
        PaneEvent::Exited(terminal) => target_for_terminal(terminal) == target,
        PaneEvent::Restore(pane) => target_for_terminal(&pane.terminal) == target,
        PaneEvent::RestoreBatch { panes, .. } => panes
            .iter()
            .all(|pane| target_for_terminal(&pane.terminal) == target),
        // A projected interrupted lineage carries its own scope, so a batch for
        // another target can never reach this entry's tabs.
        PaneEvent::RestoreInterrupted { tabs } => {
            tabs.iter().all(|tab| target_for_interrupted(tab) == target)
        }
    }
}

fn target_for_terminal(terminal: &TerminalRef) -> Target {
    terminal
        .session_id
        .map_or(Target::Root(terminal.workspace_id), Target::Session)
}

/// pane reducer が受ける TUI-local event。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// target または tab の選択を変更する。
    Select(PaneSelection),
    /// terminal / Agent の open request を placeholder として追加する。
    Request {
        /// completion と対応する durable operation。
        operation: OperationId,
        /// request を発行した target。
        target: Target,
        /// placeholder の表示種別。
        kind: PaneKind,
    },
    /// request が terminal reservation に成功した。
    Succeeded {
        /// 完了した operation。
        operation: OperationId,
        /// 完全な terminal identity。
        terminal: TerminalRef,
    },
    /// Complete a non-terminal pane without inventing a terminal identity.
    Resolved { operation: OperationId },
    /// request が失敗した。`message` は adapter が安全と保証した文言だけを渡す。
    Failed {
        /// 完了した operation。
        operation: OperationId,
        /// 画面へ出せる safe error。
        message: String,
    },
    /// daemon が terminal exit を通知した。
    Exited(TerminalRef),
    /// 保存済み terminal を再接続候補として復元する。
    Restore(LivePane),
    /// Atomically merge one target's reconciled live inventory. When
    /// `replace_order` is false, delayed results only append missing exact refs
    /// and preserve the user's newer selection/order.
    RestoreBatch {
        panes: Vec<LivePane>,
        selected: Option<TerminalRef>,
        replace_order: bool,
    },
    /// Move the selected stable tab without changing its selection identity.
    ReorderSelected(TabDirection),
    /// Close the selected pane tab. Selecting a target without a tab is a no-op.
    CloseSelected,
    /// Merge one target's projected interrupted Agent conversations (#510).
    ///
    /// The projection is authoritative for interrupted membership: a lineage it
    /// no longer carries loses its tab unless the user's own resume is still in
    /// flight for it. A retained lineage keeps its slot, its selection, and its
    /// in-flight operation while its display material is refreshed.
    RestoreInterrupted { tabs: Vec<InterruptedTab> },
    /// The user explicitly resumed one interrupted tab under `operation`. No
    /// other tab changes.
    ResumeStarted {
        continuation: AgentContinuationRef,
        operation: OperationId,
    },
    /// A validated replacement turns exactly one resuming tab live, in place.
    ResumeReplaced {
        continuation: AgentContinuationRef,
        terminal: TerminalRef,
    },
    /// One resume failed or was refused. The interrupted tab stays, carrying safe
    /// feedback.
    ///
    /// `operation` clears the in-flight marker only when it is the operation that
    /// is actually in flight, so a repeated activation refused as a duplicate can
    /// never un-pend the request it converged to.
    ResumeFailed {
        continuation: AgentContinuationRef,
        operation: Option<OperationId>,
        message: String,
    },
}

/// reducer が adapter / route reducer へ返す局所 effect。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEffect {
    /// この terminal stream を attach する。選択中の request / tab に限定する。
    Attach(TerminalRef),
    /// 最後の tab が消えたので live pane から Closeup へ戻る。
    ReturnToCloseup,
}

/// `event` を pane state へ還元し、必要な局所 effect を返す。
#[must_use]
pub fn reduce(state: &mut PaneState, event: PaneEvent) -> Vec<PaneEffect> {
    match event {
        PaneEvent::Select(selection) => {
            state.selected = selection;
            Vec::new()
        }
        PaneEvent::Request {
            operation,
            target,
            kind,
        } => request(
            state,
            PendingPane {
                operation,
                target,
                kind,
            },
        ),
        PaneEvent::Succeeded {
            operation,
            terminal,
        } => succeed(state, operation, terminal),
        PaneEvent::Resolved { operation } => resolve(state, operation),
        PaneEvent::Failed { operation, message } => fail(state, operation, message),
        PaneEvent::Exited(terminal) => exit(state, &terminal),
        PaneEvent::Restore(pane) => restore(state, pane),
        PaneEvent::RestoreBatch {
            panes,
            selected,
            replace_order,
        } => restore_batch(state, panes, selected, replace_order),
        PaneEvent::ReorderSelected(direction) => reorder_selected(state, direction),
        PaneEvent::CloseSelected => close_selected(state),
        PaneEvent::RestoreInterrupted { tabs } => restore_interrupted(state, tabs),
        PaneEvent::ResumeStarted {
            continuation,
            operation,
        } => resume_started(state, continuation, operation),
        PaneEvent::ResumeReplaced {
            continuation,
            terminal,
        } => resume_replaced(state, continuation, &terminal),
        PaneEvent::ResumeFailed {
            continuation,
            operation,
            message,
        } => resume_failed(state, continuation, operation, message),
    }
}

/// Merge one target's projected interrupted tabs (see
/// [`PaneEvent::RestoreInterrupted`]).
fn restore_interrupted(state: &mut PaneState, tabs: Vec<InterruptedTab>) -> Vec<PaneEffect> {
    let mut projected: Vec<InterruptedTab> = Vec::new();
    for tab in tabs {
        if !projected
            .iter()
            .any(|current| current.continuation == tab.continuation)
        {
            projected.push(tab);
        }
    }
    let fallback = state
        .tabs
        .iter()
        .find_map(|tab| match tab {
            PaneTab::Interrupted(pane) => Some(target_for_interrupted(&pane.tab)),
            PaneTab::Pending(_) | PaneTab::Live(_) | PaneTab::Ready(_) => None,
        })
        .or_else(|| projected.first().map(target_for_interrupted));
    state.tabs.retain(|tab| match tab {
        PaneTab::Interrupted(pane) => {
            // An in-flight explicit resume owns its tab until it answers: a
            // refresh that no longer lists the source must not make the user's
            // own request disappear.
            pane.resuming.is_some()
                || projected
                    .iter()
                    .any(|fresh| fresh.continuation == pane.tab.continuation)
        }
        PaneTab::Pending(_) | PaneTab::Live(_) | PaneTab::Ready(_) => true,
    });
    for fresh in projected {
        if let Some(existing) = state.tabs.iter_mut().find_map(|tab| match tab {
            PaneTab::Interrupted(pane) if pane.tab.continuation == fresh.continuation => Some(pane),
            PaneTab::Interrupted(_)
            | PaneTab::Pending(_)
            | PaneTab::Live(_)
            | PaneTab::Ready(_) => None,
        }) {
            existing.tab = fresh;
            continue;
        }
        state.tabs.push(PaneTab::Interrupted(InterruptedPane {
            tab: fresh,
            resuming: None,
        }));
    }
    repair_interrupted_selection(state, fallback);
    Vec::new()
}

/// Move the selection off an interrupted tab the projection removed. Every other
/// selection kind is left untouched.
fn repair_interrupted_selection(state: &mut PaneState, fallback: Option<Target>) {
    if !matches!(
        state.selected,
        PaneSelection::Tab(TabSelection::Interrupted(_))
    ) || state
        .tabs
        .iter()
        .any(|tab| selection_for(tab) == state.selected)
    {
        return;
    }
    state.selected = state.tabs.first().map_or_else(
        || {
            PaneSelection::Target(
                fallback.expect("a removed interrupted selection had an interrupted tab"),
            )
        },
        selection_for,
    );
}

fn interrupted_mut(
    state: &mut PaneState,
    continuation: AgentContinuationRef,
) -> Option<&mut InterruptedPane> {
    state.tabs.iter_mut().find_map(|tab| match tab {
        PaneTab::Interrupted(pane) if pane.tab.continuation == continuation => Some(pane),
        PaneTab::Interrupted(_) | PaneTab::Pending(_) | PaneTab::Live(_) | PaneTab::Ready(_) => {
            None
        }
    })
}

fn resume_started(
    state: &mut PaneState,
    continuation: AgentContinuationRef,
    operation: OperationId,
) -> Vec<PaneEffect> {
    let Some(pane) = interrupted_mut(state, continuation) else {
        return Vec::new();
    };
    pane.resuming = Some(operation);
    state.error = None;
    Vec::new()
}

fn resume_replaced(
    state: &mut PaneState,
    continuation: AgentContinuationRef,
    terminal: &TerminalRef,
) -> Vec<PaneEffect> {
    let Some(index) = state.tabs.iter().position(
        |tab| matches!(tab, PaneTab::Interrupted(pane) if pane.tab.continuation == continuation),
    ) else {
        return Vec::new();
    };
    let selected = state.selected == PaneSelection::Tab(TabSelection::Interrupted(continuation));
    // The replacement takes the interrupted tab's own slot, so neither the live
    // tabs nor the remaining history move.
    state.tabs[index] = PaneTab::Live(LivePane {
        terminal: terminal.clone(),
        kind: PaneKind::Agent,
    });
    if selected {
        state.selected = PaneSelection::Tab(TabSelection::Live(terminal.clone()));
    }
    state.error = None;
    // Only a foreground success attaches; a background one stays a live tab
    // until the user selects it.
    selected
        .then(|| PaneEffect::Attach(terminal.clone()))
        .into_iter()
        .collect()
}

fn resume_failed(
    state: &mut PaneState,
    continuation: AgentContinuationRef,
    operation: Option<OperationId>,
    message: String,
) -> Vec<PaneEffect> {
    let Some(pane) = interrupted_mut(state, continuation) else {
        return Vec::new();
    };
    if pane.resuming == operation {
        pane.resuming = None;
    }
    state.error = Some(message);
    Vec::new()
}

fn reorder_selected(state: &mut PaneState, direction: TabDirection) -> Vec<PaneEffect> {
    if state.tabs.len() < 2 {
        return Vec::new();
    }
    let PaneSelection::Tab(selected) = &state.selected else {
        return Vec::new();
    };
    let Some(index) = state
        .tabs
        .iter()
        .position(|tab| selection_for(tab) == PaneSelection::Tab(selected.clone()))
    else {
        return Vec::new();
    };
    let destination = match direction {
        TabDirection::Next => (index + 1) % state.tabs.len(),
        TabDirection::Previous => (index + state.tabs.len() - 1) % state.tabs.len(),
    };
    state.tabs.swap(index, destination);
    Vec::new()
}

fn close_selected(state: &mut PaneState) -> Vec<PaneEffect> {
    // Every tab kind projects its own stable selection key, so comparing against
    // it needs no per-kind product match: a target selection owns no tab and
    // therefore closes nothing.
    let Some(index) = state
        .tabs
        .iter()
        .position(|tab| selection_for(tab) == state.selected)
    else {
        return Vec::new();
    };
    let target = match state.tabs.remove(index) {
        PaneTab::Pending(pending) => pending.target,
        PaneTab::Live(live) => match live.terminal.session_id {
            Some(session) => Target::Session(session),
            None => Target::Root(live.terminal.workspace_id),
        },
        PaneTab::Ready(ready) => ready.target,
        PaneTab::Interrupted(pane) => target_for_interrupted(&pane.tab),
    };
    if state.tabs.is_empty() {
        state.selected = PaneSelection::Target(target);
        return vec![PaneEffect::ReturnToCloseup];
    }
    state.selected = selection_for(&state.tabs[index.min(state.tabs.len() - 1)]);
    Vec::new()
}

fn request(state: &mut PaneState, pending: PendingPane) -> Vec<PaneEffect> {
    if state.tabs.iter().any(
        |tab| matches!(tab, PaneTab::Pending(current) if current.operation == pending.operation),
    ) {
        return Vec::new();
    }
    state.tabs.push(PaneTab::Pending(pending));
    state.error = None;
    Vec::new()
}

fn succeed(
    state: &mut PaneState,
    operation: OperationId,
    terminal: TerminalRef,
) -> Vec<PaneEffect> {
    let Some((pending_index, pending)) =
        state
            .tabs
            .iter()
            .enumerate()
            .find_map(|(index, tab)| match tab {
                PaneTab::Pending(pending) if pending.operation == operation => {
                    Some((index, *pending))
                }
                PaneTab::Pending(_)
                | PaneTab::Live(_)
                | PaneTab::Ready(_)
                | PaneTab::Interrupted(_) => None,
            })
    else {
        return Vec::new();
    };
    let attach = matches!(
        &state.selected,
        PaneSelection::Target(target) if *target == pending.target
    ) || matches!(
        &state.selected,
        PaneSelection::Tab(TabSelection::Pending(selected)) if *selected == operation
    );
    let selected_pending = matches!(
        &state.selected,
        PaneSelection::Tab(TabSelection::Pending(selected)) if *selected == operation
    );

    if let Some(existing_index) = state
        .tabs
        .iter()
        .position(|tab| matches!(tab, PaneTab::Live(live) if live.terminal == terminal))
    {
        state.tabs.remove(pending_index);
        if selected_pending {
            state.selected = PaneSelection::Tab(TabSelection::Live(terminal.clone()));
        }
        // `existing_index` is intentionally not used for selection: tab identities,
        // rather than shifting indices, preserve the selected live tab.
        let _ = existing_index;
    } else {
        state.tabs[pending_index] = PaneTab::Live(LivePane {
            terminal: terminal.clone(),
            kind: pending.kind,
        });
        if selected_pending {
            state.selected = PaneSelection::Tab(TabSelection::Live(terminal.clone()));
        }
    }
    state.error = None;
    attach
        .then_some(PaneEffect::Attach(terminal))
        .into_iter()
        .collect()
}

fn resolve(state: &mut PaneState, operation: OperationId) -> Vec<PaneEffect> {
    let Some((index, pending)) = state
        .tabs
        .iter()
        .enumerate()
        .find_map(|(index, tab)| match tab {
            PaneTab::Pending(pending) if pending.operation == operation => Some((index, *pending)),
            PaneTab::Pending(_)
            | PaneTab::Live(_)
            | PaneTab::Ready(_)
            | PaneTab::Interrupted(_) => None,
        })
    else {
        return Vec::new();
    };
    state.tabs[index] = PaneTab::Ready(pending);
    if matches!(
        &state.selected,
        PaneSelection::Tab(TabSelection::Pending(selected)) if *selected == operation
    ) {
        state.selected = PaneSelection::Tab(TabSelection::Ready(operation));
    }
    state.error = None;
    Vec::new()
}

fn fail(state: &mut PaneState, operation: OperationId, message: String) -> Vec<PaneEffect> {
    let Some((index, target)) = state
        .tabs
        .iter()
        .enumerate()
        .find_map(|(index, tab)| match tab {
            PaneTab::Pending(pending) if pending.operation == operation => {
                Some((index, pending.target))
            }
            PaneTab::Pending(_)
            | PaneTab::Live(_)
            | PaneTab::Ready(_)
            | PaneTab::Interrupted(_) => None,
        })
    else {
        return Vec::new();
    };
    state.tabs.remove(index);
    if matches!(
        &state.selected,
        PaneSelection::Tab(TabSelection::Pending(selected)) if *selected == operation
    ) {
        state.selected = PaneSelection::Target(target);
    }
    state.error = Some(message);
    Vec::new()
}

fn exit(state: &mut PaneState, terminal: &TerminalRef) -> Vec<PaneEffect> {
    let Some(index) = state
        .tabs
        .iter()
        .position(|tab| matches!(tab, PaneTab::Live(live) if live.terminal == *terminal))
    else {
        return Vec::new();
    };
    let was_selected = matches!(
        &state.selected,
        PaneSelection::Tab(TabSelection::Live(selected)) if selected == terminal
    );
    state.tabs.remove(index);
    if state.tabs.is_empty() {
        state.selected = PaneSelection::Target(target_for_terminal(terminal));
        return vec![PaneEffect::ReturnToCloseup];
    }
    if was_selected {
        let next_index = index.min(state.tabs.len() - 1);
        state.selected = selection_for(&state.tabs[next_index]);
    }
    Vec::new()
}

fn restore(state: &mut PaneState, pane: LivePane) -> Vec<PaneEffect> {
    if state
        .tabs
        .iter()
        .any(|tab| matches!(tab, PaneTab::Live(live) if live.terminal == pane.terminal))
    {
        return Vec::new();
    }
    let attach = matches!(
        &state.selected,
        PaneSelection::Tab(TabSelection::Live(selected)) if *selected == pane.terminal
    );
    let terminal = pane.terminal.clone();
    state.tabs.push(PaneTab::Live(pane));
    attach
        .then_some(PaneEffect::Attach(terminal))
        .into_iter()
        .collect()
}

fn restore_batch(
    state: &mut PaneState,
    panes: Vec<LivePane>,
    selected: Option<TerminalRef>,
    replace_order: bool,
) -> Vec<PaneEffect> {
    let mut unique = Vec::new();
    for pane in panes {
        if !unique
            .iter()
            .any(|current: &LivePane| current.terminal.fences(&pane.terminal))
        {
            unique.push(pane);
        }
    }
    if replace_order {
        let fallback_target = state.tabs.first().map(|tab| match tab {
            PaneTab::Pending(pending) => pending.target,
            PaneTab::Live(live) => target_for_terminal(&live.terminal),
            PaneTab::Ready(ready) => ready.target,
            PaneTab::Interrupted(pane) => target_for_interrupted(&pane.tab),
        });
        let mut retained = std::mem::take(&mut state.tabs);
        let mut ordered = unique.into_iter().map(PaneTab::Live).collect::<Vec<_>>();
        // A coherent restore is authoritative for live membership. Preserve
        // only local in-flight placeholders; live tabs absent from the fresh
        // inventory (including cross-client dismissals and exits) are removed.
        // Interrupted history is projected by its own event, not by live
        // inventory, so a live restore neither drops nor reorders it.
        retained.retain(|tab| {
            matches!(
                tab,
                PaneTab::Pending(_) | PaneTab::Ready(_) | PaneTab::Interrupted(_)
            )
        });
        ordered.extend(retained);
        state.tabs = ordered;
        if let Some(selected) = selected
            && state
                .tabs
                .iter()
                .any(|tab| matches!(tab, PaneTab::Live(live) if live.terminal.fences(&selected)))
        {
            state.selected = PaneSelection::Tab(TabSelection::Live(selected));
        } else if !state
            .tabs
            .iter()
            .any(|tab| selection_for(tab) == state.selected)
        {
            state.selected = state.tabs.first().map_or_else(
                || {
                    PaneSelection::Target(fallback_target.unwrap_or_else(
                        || match &state.selected {
                            PaneSelection::None => {
                                unreachable!("target-scoped pane restore has an active target")
                            }
                            PaneSelection::Target(target) => *target,
                            PaneSelection::Tab(TabSelection::Live(terminal)) => {
                                target_for_terminal(terminal)
                            }
                            PaneSelection::Tab(
                                TabSelection::Pending(_)
                                | TabSelection::Ready(_)
                                | TabSelection::Interrupted(_),
                            ) => unreachable!("a tab selection without a target-scoped tab"),
                        },
                    ))
                },
                selection_for,
            );
        }
    } else {
        let was_empty = state.tabs.is_empty();
        for pane in unique {
            let _ = restore(state, pane);
        }
        if was_empty {
            state.selected = selected
                .filter(|selected| {
                    state.tabs.iter().any(
                        |tab| matches!(tab, PaneTab::Live(live) if live.terminal.fences(selected)),
                    )
                })
                .map_or_else(
                    || {
                        state
                            .tabs
                            .first()
                            .map_or_else(|| state.selected.clone(), selection_for)
                    },
                    |selected| PaneSelection::Tab(TabSelection::Live(selected)),
                );
        }
    }
    Vec::new()
}

fn selection_for(tab: &PaneTab) -> PaneSelection {
    match tab {
        PaneTab::Pending(pending) => PaneSelection::Tab(TabSelection::Pending(pending.operation)),
        PaneTab::Live(live) => PaneSelection::Tab(TabSelection::Live(live.terminal.clone())),
        PaneTab::Ready(ready) => PaneSelection::Tab(TabSelection::Ready(ready.operation)),
        PaneTab::Interrupted(pane) => {
            PaneSelection::Tab(TabSelection::Interrupted(pane.tab.continuation))
        }
    }
}

/// The pane target an interrupted lineage belongs to. It mirrors
/// [`target_for_terminal`] so root and managed-session histories land in the same
/// registry entry as their live siblings.
fn target_for_interrupted(tab: &InterruptedTab) -> Target {
    tab.session_id.map_or(
        Target::Root(tab.last_terminal.workspace_id),
        Target::Session,
    )
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use usagi_core::domain::id::{
        DaemonGeneration, SessionId, TerminalId, WorkspaceId, WorktreeId,
    };

    use super::*;

    fn target() -> Target {
        Target::Session(SessionId::new())
    }

    fn terminal(target: Target) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: match target {
                Target::Root(workspace) => workspace,
                Target::Session(_) => WorkspaceId::new(),
            },
            session_id: match target {
                Target::Root(_) => None,
                Target::Session(session) => Some(session),
            },
            worktree_id: WorktreeId::new(),
        }
    }

    fn request(state: &mut PaneState, target: Target, kind: PaneKind) -> OperationId {
        let operation = OperationId::new();
        assert!(
            reduce(
                state,
                PaneEvent::Request {
                    operation,
                    target,
                    kind,
                },
            )
            .is_empty()
        );
        operation
    }

    #[test]
    fn selected_target_turns_terminal_and_agent_placeholders_into_live_attached_tabs() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let terminal_operation = request(&mut state, target, PaneKind::Terminal);
        let agent_operation = request(&mut state, target, PaneKind::Agent);
        let terminal_ref = terminal(target);
        let agent_ref = terminal(target);

        assert_eq!(
            reduce(
                &mut state,
                PaneEvent::Succeeded {
                    operation: terminal_operation,
                    terminal: terminal_ref.clone(),
                },
            ),
            vec![PaneEffect::Attach(terminal_ref.clone())]
        );
        assert_eq!(
            reduce(
                &mut state,
                PaneEvent::Succeeded {
                    operation: agent_operation,
                    terminal: agent_ref.clone(),
                },
            ),
            vec![PaneEffect::Attach(agent_ref.clone())]
        );
        assert_eq!(
            state.tabs(),
            &[
                PaneTab::Live(LivePane {
                    terminal: terminal_ref,
                    kind: PaneKind::Terminal,
                }),
                PaneTab::Live(LivePane {
                    terminal: agent_ref,
                    kind: PaneKind::Agent,
                }),
            ]
        );
    }

    #[test]
    fn selected_diff_placeholder_becomes_a_ready_tab_without_a_terminal_identity() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let operation = request(&mut state, target, PaneKind::Diff);
        let _ = reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Pending(operation))),
        );

        assert!(reduce(&mut state, PaneEvent::Resolved { operation }).is_empty());
        assert!(matches!(
            state.tabs(),
            [PaneTab::Ready(ready)] if ready.kind == PaneKind::Diff && ready.operation == operation
        ));
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Ready(operation))
        );
    }

    #[test]
    fn closing_the_selected_pending_tab_selects_the_next_tab_then_returns_to_target() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let first = request(&mut state, target, PaneKind::Terminal);
        let second = request(&mut state, target, PaneKind::Agent);
        let _ = reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Pending(first))),
        );

        assert!(reduce(&mut state, PaneEvent::CloseSelected).is_empty());
        assert_eq!(state.tabs().len(), 1);
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Pending(second))
        );
        assert_eq!(
            reduce(&mut state, PaneEvent::CloseSelected),
            vec![PaneEffect::ReturnToCloseup]
        );
        assert_eq!(state.selected(), &PaneSelection::Target(target));
    }

    #[test]
    fn selected_placeholder_reuses_a_live_tab_and_preserves_its_stable_selection() {
        let target = target();
        let existing = terminal(target);
        let operation = OperationId::new();
        let mut state = PaneState::with_live(
            PaneSelection::Tab(TabSelection::Pending(operation)),
            vec![LivePane {
                terminal: existing.clone(),
                kind: PaneKind::Terminal,
            }],
        );
        let _ = reduce(
            &mut state,
            PaneEvent::Request {
                operation,
                target,
                kind: PaneKind::Terminal,
            },
        );

        assert_eq!(
            reduce(
                &mut state,
                PaneEvent::Succeeded {
                    operation,
                    terminal: existing.clone(),
                },
            ),
            vec![PaneEffect::Attach(existing.clone())]
        );
        assert_eq!(state.tabs().len(), 1);
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(existing))
        );
    }

    #[test]
    fn failure_removes_placeholder_keeps_other_tabs_and_records_safe_error() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let retained = terminal(target);
        let _ = reduce(
            &mut state,
            PaneEvent::Restore(LivePane {
                terminal: retained.clone(),
                kind: PaneKind::Terminal,
            }),
        );
        let operation = request(&mut state, target, PaneKind::Agent);
        let _ = reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Pending(operation))),
        );

        assert!(
            reduce(
                &mut state,
                PaneEvent::Failed {
                    operation,
                    message: "agent failed safely".to_owned(),
                },
            )
            .is_empty()
        );
        assert_eq!(state.tabs().len(), 1);
        assert_eq!(state.error(), Some("agent failed safely"));
        assert_eq!(state.selected(), &PaneSelection::Target(target));
    }

    #[test]
    fn exit_selects_next_tab_or_returns_to_closeup_after_the_last_tab() {
        let target = target();
        let first = terminal(target);
        let second = terminal(target);
        let mut state = PaneState::with_live(
            PaneSelection::Tab(TabSelection::Live(first.clone())),
            vec![
                LivePane {
                    terminal: first.clone(),
                    kind: PaneKind::Terminal,
                },
                LivePane {
                    terminal: second.clone(),
                    kind: PaneKind::Agent,
                },
            ],
        );

        assert!(reduce(&mut state, PaneEvent::Exited(first)).is_empty());
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(second.clone()))
        );
        assert_eq!(
            reduce(&mut state, PaneEvent::Exited(second)),
            vec![PaneEffect::ReturnToCloseup]
        );
    }

    #[test]
    fn completion_and_exit_in_the_background_never_steal_the_selected_pane() {
        let requested = target();
        let background = target();
        let selected = terminal(background);
        let mut state = PaneState::with_live(
            PaneSelection::Tab(TabSelection::Live(selected.clone())),
            vec![LivePane {
                terminal: selected.clone(),
                kind: PaneKind::Terminal,
            }],
        );
        let operation = request(&mut state, requested, PaneKind::Agent);
        let opened = terminal(requested);

        assert!(
            reduce(
                &mut state,
                PaneEvent::Succeeded {
                    operation,
                    terminal: opened.clone(),
                },
            )
            .is_empty()
        );
        assert!(
            reduce(
                &mut state,
                PaneEvent::Resolved {
                    operation: OperationId::new(),
                },
            )
            .is_empty()
        );
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(selected.clone()))
        );
        assert!(reduce(&mut state, PaneEvent::Exited(opened)).is_empty());
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(selected))
        );
    }

    #[test]
    fn restored_selected_terminal_reattaches_but_other_input_does_not_cancel_pending_work() {
        let target = target();
        let saved = terminal(target);
        let mut state = PaneState::new(PaneSelection::Tab(TabSelection::Live(saved.clone())));
        assert_eq!(
            reduce(
                &mut state,
                PaneEvent::Restore(LivePane {
                    terminal: saved.clone(),
                    kind: PaneKind::Terminal,
                }),
            ),
            vec![PaneEffect::Attach(saved)]
        );

        let operation = request(&mut state, target, PaneKind::Agent);
        let _ = reduce(&mut state, PaneEvent::Select(PaneSelection::Target(target)));
        assert!(state.tabs().iter().any(|tab| {
            matches!(tab, PaneTab::Pending(pending) if pending.operation == operation)
        }));
    }

    #[test]
    fn restore_batch_deduplicates_and_late_replay_preserves_newer_order_and_selection() {
        let target = target();
        let first = terminal(target);
        let second = terminal(target);
        let discovered = terminal(target);
        let mut state = PaneState::new(PaneSelection::Target(target));
        let panes = vec![
            LivePane {
                terminal: second.clone(),
                kind: PaneKind::Agent,
            },
            LivePane {
                terminal: first.clone(),
                kind: PaneKind::Agent,
            },
            LivePane {
                terminal: first.clone(),
                kind: PaneKind::Agent,
            },
        ];
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes,
                selected: Some(first.clone()),
                replace_order: true,
            },
        );
        assert_eq!(state.tabs().len(), 2);
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(first))
        );

        let _ = reduce(
            &mut state,
            PaneEvent::ReorderSelected(TabDirection::Previous),
        );
        let selection = state.selected().clone();
        let order = state.tabs().to_vec();
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes: vec![
                    LivePane {
                        terminal: second,
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: discovered,
                        kind: PaneKind::Agent,
                    },
                ],
                selected: None,
                replace_order: false,
            },
        );
        assert_eq!(state.selected(), &selection);
        assert_eq!(&state.tabs()[..2], order.as_slice());
        assert_eq!(state.tabs().len(), 3);
    }

    #[test]
    fn append_restore_selects_the_first_live_tab_for_an_empty_target() {
        let target = target();
        let first = terminal(target);
        let second = terminal(target);
        let mut state = PaneState::new(PaneSelection::Target(target));

        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes: vec![
                    LivePane {
                        terminal: first.clone(),
                        kind: PaneKind::Terminal,
                    },
                    LivePane {
                        terminal: second,
                        kind: PaneKind::Agent,
                    },
                ],
                selected: None,
                replace_order: false,
            },
        );

        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(first))
        );

        let mut still_empty = PaneState::new(PaneSelection::Target(target));
        let _ = reduce(
            &mut still_empty,
            PaneEvent::RestoreBatch {
                panes: Vec::new(),
                selected: None,
                replace_order: false,
            },
        );
        assert_eq!(still_empty.selected(), &PaneSelection::Target(target));
    }

    #[test]
    fn append_restore_honours_a_valid_selection_for_an_empty_target() {
        let target = target();
        let first = terminal(target);
        let selected = terminal(target);
        let mut state = PaneState::new(PaneSelection::Target(target));

        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes: vec![
                    LivePane {
                        terminal: first,
                        kind: PaneKind::Terminal,
                    },
                    LivePane {
                        terminal: selected.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(selected.clone()),
                replace_order: false,
            },
        );

        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(selected))
        );
    }

    #[test]
    fn authoritative_restore_removes_absent_live_tabs_but_preserves_pending_work() {
        let target = target();
        let stale_terminal = terminal(target);
        let current = terminal(target);
        let operation = OperationId::new();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let _ = reduce(
            &mut state,
            PaneEvent::Restore(LivePane {
                terminal: stale_terminal.clone(),
                kind: PaneKind::Agent,
            }),
        );
        let _ = reduce(
            &mut state,
            PaneEvent::Request {
                operation,
                target,
                kind: PaneKind::Terminal,
            },
        );
        let _ = reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(stale_terminal))),
        );

        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes: vec![LivePane {
                    terminal: current.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(current.clone()),
                replace_order: true,
            },
        );
        assert_eq!(state.tabs().len(), 2);
        assert!(matches!(
            &state.tabs()[0],
            PaneTab::Live(live) if live.terminal.fences(&current)
        ));
        assert!(matches!(
            &state.tabs()[1],
            PaneTab::Pending(pending) if pending.operation == operation
        ));

        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes: Vec::new(),
                selected: None,
                replace_order: true,
            },
        );
        assert!(
            matches!(state.tabs(), [PaneTab::Pending(pending)] if pending.operation == operation)
        );
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Pending(operation))
        );
    }

    #[test]
    fn reorder_noops_and_authoritative_empty_restore_cover_selection_fallbacks() {
        let target = target();
        let first = terminal(target);
        let second = terminal(target);

        let mut too_short = PaneState::new(PaneSelection::Target(target));
        assert!(
            reduce(
                &mut too_short,
                PaneEvent::ReorderSelected(TabDirection::Next),
            )
            .is_empty()
        );

        let live_tabs = vec![
            PaneTab::Live(LivePane {
                terminal: first.clone(),
                kind: PaneKind::Agent,
            }),
            PaneTab::Live(LivePane {
                terminal: second,
                kind: PaneKind::Agent,
            }),
        ];
        let mut target_selected = PaneState::new(PaneSelection::Target(target));
        target_selected.tabs = live_tabs.clone();
        let _ = reduce(
            &mut target_selected,
            PaneEvent::ReorderSelected(TabDirection::Next),
        );
        assert_eq!(target_selected.tabs, live_tabs);

        let mut stale_selected =
            PaneState::new(PaneSelection::Tab(TabSelection::Live(terminal(target))));
        stale_selected.tabs = live_tabs.clone();
        let stale_selection = stale_selected.selected.clone();
        assert!(
            reduce(
                &mut stale_selected,
                PaneEvent::ReorderSelected(TabDirection::Next),
            )
            .is_empty()
        );
        assert_eq!(stale_selected.tabs, live_tabs);
        assert_eq!(stale_selected.selected, stale_selection);

        for tab in [
            PaneTab::Pending(PendingPane {
                operation: OperationId::new(),
                target,
                kind: PaneKind::Agent,
            }),
            PaneTab::Ready(PendingPane {
                operation: OperationId::new(),
                target,
                kind: PaneKind::Agent,
            }),
        ] {
            let mut retained = PaneState::new(PaneSelection::Target(target));
            retained.tabs.push(tab);
            let _ = reduce(
                &mut retained,
                PaneEvent::RestoreBatch {
                    panes: Vec::new(),
                    selected: None,
                    replace_order: true,
                },
            );
            assert!(matches!(retained.selected(), PaneSelection::Tab(_)));
        }

        let mut empty = PaneState::new(PaneSelection::Tab(TabSelection::Live(first.clone())));
        let _ = reduce(
            &mut empty,
            PaneEvent::RestoreBatch {
                panes: Vec::new(),
                selected: None,
                replace_order: true,
            },
        );
        assert_eq!(empty.selected(), &PaneSelection::Target(target));

        let retained_terminal = terminal(target);
        let mut retained_selection = PaneState::new(PaneSelection::Tab(TabSelection::Live(
            retained_terminal.clone(),
        )));
        retained_selection.tabs.push(PaneTab::Live(LivePane {
            terminal: retained_terminal.clone(),
            kind: PaneKind::Agent,
        }));
        let _ = reduce(
            &mut retained_selection,
            PaneEvent::RestoreBatch {
                panes: vec![LivePane {
                    terminal: retained_terminal.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: None,
                replace_order: true,
            },
        );
        assert_eq!(
            retained_selection.selected(),
            &PaneSelection::Tab(TabSelection::Live(retained_terminal))
        );
    }

    #[test]
    #[should_panic(expected = "a tab selection without a target-scoped tab")]
    fn authoritative_empty_restore_rejects_an_impossible_pending_selection() {
        let mut state = PaneState::new(PaneSelection::Tab(TabSelection::Pending(
            OperationId::new(),
        )));
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes: Vec::new(),
                selected: None,
                replace_order: true,
            },
        );
    }

    #[test]
    fn selected_tab_reorder_wraps_and_registry_revision_only_tracks_effective_mutation() {
        let target = target();
        let first = terminal(target);
        let second = terminal(target);
        let mut registry = PaneRegistry::new(target);
        let initial_revision = registry.revision();
        let _ = route_tab_command(&mut registry, PaneTabCommand::Reorder(TabDirection::Next));
        assert_eq!(registry.revision(), initial_revision);
        for terminal in [first.clone(), second.clone()] {
            let _ = reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target,
                    event: PaneEvent::Restore(LivePane {
                        terminal,
                        kind: PaneKind::Agent,
                    }),
                },
            );
        }
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(first.clone()))),
            },
        );
        let before = registry.revision();
        let _ = route_tab_command(
            &mut registry,
            PaneTabCommand::Reorder(TabDirection::Previous),
        );
        assert_eq!(registry.revision(), before + 1);
        assert!(matches!(
            registry.active_pane().tabs(),
            [PaneTab::Live(left), PaneTab::Live(right)]
                if left.terminal == second && right.terminal == first
        ));
    }

    #[test]
    fn stale_and_duplicate_events_are_inert_and_selected_pending_promotes_to_live() {
        let workspace = WorkspaceId::new();
        let root = Target::Root(workspace);
        let target = target();
        let pending = OperationId::new();
        let next_pending = OperationId::new();
        let live = terminal(root);
        let mut state = PaneState::new(PaneSelection::Tab(TabSelection::Pending(pending)));

        let _ = reduce(
            &mut state,
            PaneEvent::Request {
                operation: pending,
                target: root,
                kind: PaneKind::Terminal,
            },
        );
        assert!(
            reduce(
                &mut state,
                PaneEvent::Request {
                    operation: pending,
                    target: root,
                    kind: PaneKind::Terminal,
                },
            )
            .is_empty()
        );
        assert_eq!(
            reduce(
                &mut state,
                PaneEvent::Succeeded {
                    operation: pending,
                    terminal: live.clone(),
                },
            ),
            vec![PaneEffect::Attach(live.clone())]
        );
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(live.clone()))
        );
        assert!(
            reduce(
                &mut state,
                PaneEvent::Succeeded {
                    operation: OperationId::new(),
                    terminal: terminal(target),
                },
            )
            .is_empty()
        );
        assert!(
            reduce(
                &mut state,
                PaneEvent::Failed {
                    operation: OperationId::new(),
                    message: "stale failure".to_owned(),
                },
            )
            .is_empty()
        );
        assert!(reduce(&mut state, PaneEvent::Exited(terminal(target))).is_empty());
        assert!(
            reduce(
                &mut state,
                PaneEvent::Restore(LivePane {
                    terminal: live.clone(),
                    kind: PaneKind::Terminal,
                }),
            )
            .is_empty()
        );

        let _ = reduce(
            &mut state,
            PaneEvent::Request {
                operation: next_pending,
                target,
                kind: PaneKind::Agent,
            },
        );
        assert!(reduce(&mut state, PaneEvent::Exited(live)).is_empty());
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Pending(next_pending))
        );
    }

    fn registry_request(
        registry: &mut PaneRegistry,
        target: Target,
        kind: PaneKind,
    ) -> OperationId {
        let operation = OperationId::new();
        assert!(
            reduce_registry(
                registry,
                PaneRegistryEvent::Pane {
                    target,
                    event: PaneEvent::Request {
                        operation,
                        target,
                        kind,
                    },
                },
            )
            .is_empty()
        );
        operation
    }

    #[test]
    fn registry_keeps_sessions_pending_tabs_selection_and_modal_state_isolated() {
        let session_a = target();
        let session_b = target();
        let mut registry = PaneRegistry::new(session_a);
        let pending_a = registry_request(&mut registry, session_a, PaneKind::Terminal);
        let terminal_a = terminal(session_a);
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target: session_a,
                event: PaneEvent::Succeeded {
                    operation: pending_a,
                    terminal: terminal_a.clone(),
                },
            },
        );
        let _ = reduce_registry(&mut registry, PaneRegistryEvent::SelectTarget(session_b));
        let pending_b = registry_request(&mut registry, session_b, PaneKind::Agent);
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::OpenActionModal { target: session_b },
        );

        // Background exit changes A only; B's pending tab and forced modal stay put.
        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target: session_a,
                    event: PaneEvent::Exited(terminal_a.clone()),
                },
            )
            .is_empty()
        );
        assert_eq!(registry.active(), Some(session_b));
        assert!(registry.action_modal_visible(session_b));
        assert!(
            registry.pane(session_b).unwrap().tabs().iter().any(
                |tab| matches!(tab, PaneTab::Pending(pending) if pending.operation == pending_b)
            )
        );

        let _ = reduce_registry(&mut registry, PaneRegistryEvent::SelectTarget(session_a));
        assert!(!registry.pane(session_a).unwrap().has_tabs());
        assert!(registry.action_modal_visible(session_a));
        let _ = reduce_registry(&mut registry, PaneRegistryEvent::SelectTarget(session_b));
        assert_eq!(
            registry.pane(session_b).unwrap().selected(),
            &PaneSelection::Target(session_b)
        );
    }

    #[test]
    fn registry_background_completion_and_close_do_not_change_visible_target() {
        let visible = target();
        let background = target();
        let mut registry = PaneRegistry::new(visible);
        let visible_operation = registry_request(&mut registry, visible, PaneKind::Terminal);
        let visible_terminal = terminal(visible);
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target: visible,
                event: PaneEvent::Succeeded {
                    operation: visible_operation,
                    terminal: visible_terminal.clone(),
                },
            },
        );
        let background_operation = registry_request(&mut registry, background, PaneKind::Agent);
        let background_terminal = terminal(background);

        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target: background,
                    event: PaneEvent::Succeeded {
                        operation: background_operation,
                        terminal: background_terminal.clone(),
                    },
                },
            )
            .is_empty()
        );
        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target: background,
                    event: PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(
                        background_terminal,
                    ))),
                },
            )
            .is_empty()
        );
        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target: background,
                    event: PaneEvent::CloseSelected,
                },
            )
            .is_empty()
        );

        assert_eq!(registry.active(), Some(visible));
        assert_eq!(registry.input_owner(), PaneInputOwner::Tab);
        assert_eq!(
            registry.active_pane().selected(),
            &PaneSelection::Target(visible)
        );
        assert_eq!(
            registry.active_pane().tabs(),
            &[PaneTab::Live(LivePane {
                terminal: visible_terminal,
                kind: PaneKind::Terminal,
            })]
        );
    }

    #[test]
    fn modal_visibility_and_tab_input_ownership_follow_the_reducer_table() {
        let target = target();
        let mut registry = PaneRegistry::new(target);
        assert_eq!(registry.input_owner(), PaneInputOwner::ActionModal);
        assert!(route_tab_command(&mut registry, PaneTabCommand::Passthrough).is_empty());

        let operation = registry_request(&mut registry, target, PaneKind::Terminal);
        assert_eq!(registry.input_owner(), PaneInputOwner::Tab);
        assert!(
            route_tab_command(
                &mut registry,
                PaneTabCommand::Select(TabSelection::Pending(operation)),
            )
            .is_empty()
        );
        assert_eq!(
            route_tab_command(&mut registry, PaneTabCommand::Passthrough),
            vec![PaneRegistryEffect::Passthrough { target }]
        );

        let _ = reduce_registry(&mut registry, PaneRegistryEvent::OpenActionModal { target });
        assert_eq!(registry.input_owner(), PaneInputOwner::ActionModal);
        assert!(route_tab_command(&mut registry, PaneTabCommand::Close).is_empty());
        assert!(registry.pane(target).unwrap().has_tabs());

        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::CloseActionModal { target },
        );
        assert_eq!(registry.input_owner(), PaneInputOwner::Tab);
        assert_eq!(
            route_tab_command(&mut registry, PaneTabCommand::Close),
            vec![PaneRegistryEffect::Pane {
                target,
                effect: PaneEffect::ReturnToCloseup,
            }]
        );
        assert!(!registry.pane(target).unwrap().has_tabs());
        assert_eq!(registry.input_owner(), PaneInputOwner::ActionModal);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One pane fixture keeps target fences and close transitions ordered.
    fn coverage_contract_covers_target_fences_ready_and_root_live_close() {
        let target = target();
        let foreign = Target::Session(SessionId::new());
        let mut registry = PaneRegistry::new(target);
        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target,
                    event: PaneEvent::Request {
                        operation: OperationId::new(),
                        target: foreign,
                        kind: PaneKind::Terminal,
                    },
                },
            )
            .is_empty()
        );
        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target,
                    event: PaneEvent::Restore(LivePane {
                        terminal: terminal(foreign),
                        kind: PaneKind::Terminal,
                    }),
                },
            )
            .is_empty()
        );

        let operation = registry_request(&mut registry, target, PaneKind::Diff);
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Resolved { operation },
            },
        );
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(terminal(target)))),
            },
        );
        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target,
                    event: PaneEvent::CloseSelected,
                },
            )
            .is_empty()
        );
        let _ = route_tab_command(
            &mut registry,
            PaneTabCommand::Select(TabSelection::Ready(operation)),
        );
        assert_eq!(
            route_tab_command(&mut registry, PaneTabCommand::Close),
            vec![PaneRegistryEffect::Pane {
                target,
                effect: PaneEffect::ReturnToCloseup,
            }]
        );

        let workspace = WorkspaceId::new();
        let root = Target::Root(workspace);
        let mut state = PaneState::new(PaneSelection::Target(root));
        let root_terminal = TerminalRef {
            workspace_id: workspace,
            worktree_id: WorktreeId::new(),
            session_id: None,
            terminal_id: TerminalId::new(),
            daemon_generation: DaemonGeneration::new(),
        };
        let _ = reduce(
            &mut state,
            PaneEvent::Restore(LivePane {
                terminal: root_terminal.clone(),
                kind: PaneKind::Terminal,
            }),
        );
        let _ = reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(root_terminal))),
        );
        assert_eq!(
            reduce(&mut state, PaneEvent::CloseSelected),
            vec![PaneEffect::ReturnToCloseup]
        );
        assert!(reduce(&mut state, PaneEvent::CloseSelected).is_empty());

        let mut ready = PaneState::new(PaneSelection::Target(target));
        let first = request(&mut ready, target, PaneKind::Diff);
        let second = request(&mut ready, target, PaneKind::Diff);
        let _ = reduce(&mut ready, PaneEvent::Resolved { operation: first });
        let _ = reduce(&mut ready, PaneEvent::Resolved { operation: second });
        let _ = reduce(
            &mut ready,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Ready(first))),
        );
        assert!(reduce(&mut ready, PaneEvent::CloseSelected).is_empty());
        assert_eq!(
            ready.selected(),
            &PaneSelection::Tab(TabSelection::Ready(second))
        );
    }

    /// One projected interrupted lineage for `target`, resumable unless
    /// `resumable` is false.
    fn interrupted(target: Target, resumable: bool) -> InterruptedTab {
        use usagi_core::domain::agent::{ProviderKind, ProviderResumePhase, ProviderResumeReason};
        use usagi_core::domain::id::{AgentResumeSourceId, AgentRuntimeId};

        let terminal = terminal(target);
        let continuation = AgentContinuationRef::new();
        let runtime_id = AgentRuntimeId::new();
        InterruptedTab {
            continuation,
            session_id: terminal.session_id,
            provider: Some(ProviderKind::Claude),
            last_known_phase: Some(ProviderResumePhase::Interrupted),
            reason: if resumable {
                ProviderResumeReason::ExplicitResumeAvailable
            } else {
                ProviderResumeReason::ProviderMetadataUnavailable
            },
            target: resumable.then(|| usagi_core::domain::agent::AgentResumeTarget {
                continuation,
                source: AgentResumeSourceId::new(),
                workspace_id: terminal.workspace_id,
                session_id: terminal.session_id,
                worktree_id: terminal.worktree_id,
                runtime_id,
                adapter_revision: 3,
            }),
            last_terminal: terminal,
        }
    }

    fn interrupted_tabs(state: &PaneState) -> Vec<AgentContinuationRef> {
        state
            .tabs()
            .iter()
            .filter_map(|tab| match tab {
                PaneTab::Interrupted(pane) => Some(pane.tab.continuation),
                PaneTab::Pending(_) | PaneTab::Live(_) | PaneTab::Ready(_) => None,
            })
            .collect()
    }

    fn resuming(state: &PaneState, continuation: AgentContinuationRef) -> Option<OperationId> {
        state.tabs().iter().find_map(|tab| match tab {
            PaneTab::Interrupted(pane) if pane.tab.continuation == continuation => pane.resuming,
            PaneTab::Interrupted(_)
            | PaneTab::Pending(_)
            | PaneTab::Live(_)
            | PaneTab::Ready(_) => None,
        })
    }

    #[test]
    fn interrupted_projection_adds_refreshes_and_dedupes_history_tabs() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let first = interrupted(target, true);
        let second = interrupted(target, true);

        assert!(
            reduce(
                &mut state,
                PaneEvent::RestoreInterrupted {
                    // A duplicated inventory row must not create a second tab.
                    tabs: vec![first.clone(), second.clone(), first.clone()],
                },
            )
            .is_empty()
        );
        assert_eq!(
            interrupted_tabs(&state),
            vec![first.continuation, second.continuation]
        );

        // A refresh replaces display material in place, keeping both slots.
        let mut refreshed = first.clone();
        refreshed.reason = usagi_core::domain::agent::ProviderResumeReason::SourceAlreadySuperseded;
        refreshed.target = None;
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreInterrupted {
                tabs: vec![refreshed, second.clone()],
            },
        );
        assert_eq!(
            interrupted_tabs(&state),
            vec![first.continuation, second.continuation]
        );
        assert!(matches!(
            &state.tabs()[0],
            PaneTab::Interrupted(pane) if !pane.tab.resumable()
        ));
    }

    #[test]
    fn interrupted_projection_drops_absent_history_but_keeps_an_in_flight_resume() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let kept = interrupted(target, true);
        let dropped = interrupted(target, true);
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreInterrupted {
                tabs: vec![kept.clone(), dropped.clone()],
            },
        );
        let operation = OperationId::new();
        let _ = reduce(
            &mut state,
            PaneEvent::ResumeStarted {
                continuation: kept.continuation,
                operation,
            },
        );

        let _ = reduce(
            &mut state,
            PaneEvent::RestoreInterrupted { tabs: Vec::new() },
        );

        // The user's own in-flight request keeps its tab; the untouched lineage
        // the projection no longer lists loses its tab.
        assert_eq!(interrupted_tabs(&state), vec![kept.continuation]);
        assert_eq!(resuming(&state, kept.continuation), Some(operation));
    }

    #[test]
    fn a_removed_interrupted_selection_falls_back_to_a_tab_then_to_its_target() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let live = terminal(target);
        let history = interrupted(target, true);
        let _ = reduce(
            &mut state,
            PaneEvent::Restore(LivePane {
                terminal: live.clone(),
                kind: PaneKind::Agent,
            }),
        );
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreInterrupted {
                tabs: vec![history.clone()],
            },
        );
        let _ = reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Interrupted(
                history.continuation,
            ))),
        );

        let _ = reduce(
            &mut state,
            PaneEvent::RestoreInterrupted { tabs: Vec::new() },
        );
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(live.clone()))
        );

        // Without any other tab the selection returns to the lineage's own scope.
        let mut only = PaneState::new(PaneSelection::Target(target));
        let _ = reduce(
            &mut only,
            PaneEvent::RestoreInterrupted {
                tabs: vec![history.clone()],
            },
        );
        let _ = reduce(
            &mut only,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Interrupted(
                history.continuation,
            ))),
        );
        let _ = reduce(
            &mut only,
            PaneEvent::RestoreInterrupted { tabs: Vec::new() },
        );
        assert_eq!(only.selected(), &PaneSelection::Target(target));
        assert!(!only.has_tabs());
    }

    #[test]
    fn a_root_history_tab_belongs_to_the_root_target_and_a_live_restore_keeps_it() {
        let workspace = WorkspaceId::new();
        let root = Target::Root(workspace);
        let history = interrupted(root, true);
        assert_eq!(target_for_interrupted(&history), root);

        let mut registry = PaneRegistry::new(root);
        // An interrupted batch for another target cannot touch this entry.
        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target: Target::Session(SessionId::new()),
                    event: PaneEvent::RestoreInterrupted {
                        tabs: vec![history.clone()],
                    },
                },
            )
            .is_empty()
        );
        assert!(!registry.active_pane().has_tabs());

        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target: root,
                event: PaneEvent::RestoreInterrupted {
                    tabs: vec![history.clone()],
                },
            },
        );
        let live = terminal(root);
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target: root,
                event: PaneEvent::RestoreBatch {
                    panes: vec![LivePane {
                        terminal: live.clone(),
                        kind: PaneKind::Agent,
                    }],
                    selected: Some(live),
                    replace_order: true,
                },
            },
        );
        // A live restore owns live membership only: the history tab survives it.
        assert_eq!(
            interrupted_tabs(registry.active_pane()),
            vec![history.continuation]
        );
    }

    #[test]
    fn a_selected_interrupted_tab_is_replaced_in_place_and_attaches_only_in_foreground() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let other = interrupted(target, true);
        let resumed = interrupted(target, true);
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreInterrupted {
                tabs: vec![other.clone(), resumed.clone()],
            },
        );
        let _ = reduce(
            &mut state,
            PaneEvent::Select(PaneSelection::Tab(TabSelection::Interrupted(
                resumed.continuation,
            ))),
        );
        let replacement = terminal(target);

        assert_eq!(
            reduce(
                &mut state,
                PaneEvent::ResumeReplaced {
                    continuation: resumed.continuation,
                    terminal: replacement.clone(),
                },
            ),
            vec![PaneEffect::Attach(replacement.clone())]
        );
        assert_eq!(
            state.selected(),
            &PaneSelection::Tab(TabSelection::Live(replacement.clone()))
        );
        // The replacement took the history tab's own slot; the other tab is
        // untouched.
        assert!(matches!(
            &state.tabs()[1],
            PaneTab::Live(live) if live.terminal == replacement && live.kind == PaneKind::Agent
        ));
        assert_eq!(interrupted_tabs(&state), vec![other.continuation]);

        // A background success stays a live tab without attaching.
        let background = terminal(target);
        assert!(
            reduce(
                &mut state,
                PaneEvent::ResumeReplaced {
                    continuation: other.continuation,
                    terminal: background,
                },
            )
            .is_empty()
        );
        assert!(interrupted_tabs(&state).is_empty());
    }

    #[test]
    fn resume_progress_for_an_unknown_lineage_changes_nothing() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let absent = AgentContinuationRef::new();
        let before = state.clone();

        for event in [
            PaneEvent::ResumeStarted {
                continuation: absent,
                operation: OperationId::new(),
            },
            PaneEvent::ResumeReplaced {
                continuation: absent,
                terminal: terminal(target),
            },
            PaneEvent::ResumeFailed {
                continuation: absent,
                operation: None,
                message: "ignored".to_owned(),
            },
        ] {
            assert!(reduce(&mut state, event).is_empty());
        }
        assert_eq!(state, before);
    }

    #[test]
    fn a_failed_resume_keeps_the_history_tab_and_shows_safe_feedback() {
        let target = target();
        let mut state = PaneState::new(PaneSelection::Target(target));
        let history = interrupted(target, true);
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreInterrupted {
                tabs: vec![history.clone()],
            },
        );
        let operation = OperationId::new();
        let _ = reduce(
            &mut state,
            PaneEvent::ResumeStarted {
                continuation: history.continuation,
                operation,
            },
        );

        // A refusal that does not name the in-flight operation reports feedback
        // without releasing the pending request.
        let _ = reduce(
            &mut state,
            PaneEvent::ResumeFailed {
                continuation: history.continuation,
                operation: None,
                message: "This conversation is already being resumed".to_owned(),
            },
        );
        assert_eq!(resuming(&state, history.continuation), Some(operation));

        assert!(
            reduce(
                &mut state,
                PaneEvent::ResumeFailed {
                    continuation: history.continuation,
                    operation: Some(operation),
                    message: "This conversation cannot be resumed".to_owned(),
                },
            )
            .is_empty()
        );
        assert_eq!(interrupted_tabs(&state), vec![history.continuation]);
        assert_eq!(resuming(&state, history.continuation), None);
        assert_eq!(state.error(), Some("This conversation cannot be resumed"));
    }

    #[test]
    fn closing_a_selected_history_tab_removes_only_that_tab() {
        let target = target();
        let mut registry = PaneRegistry::new(target);
        let first = interrupted(target, true);
        let second = interrupted(target, true);
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::RestoreInterrupted {
                    tabs: vec![first.clone(), second.clone()],
                },
            },
        );
        let _ = reduce_registry(
            &mut registry,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Select(PaneSelection::Tab(TabSelection::Interrupted(
                    first.continuation,
                ))),
            },
        );

        let _ = route_tab_command(&mut registry, PaneTabCommand::Close);
        assert_eq!(
            interrupted_tabs(registry.active_pane()),
            vec![second.continuation]
        );

        // Closing the last one returns to the target's Closeup surface.
        assert_eq!(
            route_tab_command(&mut registry, PaneTabCommand::Close),
            vec![PaneRegistryEffect::Pane {
                target,
                effect: PaneEffect::ReturnToCloseup,
            }]
        );
        assert_eq!(
            registry.active_pane().selected(),
            &PaneSelection::Target(target)
        );
    }

    #[test]
    fn optional_registry_state_is_inert_and_live_inventory_skips_non_live_tabs() {
        let target = target();
        let operation = OperationId::new();
        let live = terminal(target);
        let history = interrupted(target, true);
        let mut registry = PaneRegistry::new(target);
        registry.entries[0].pane.tabs = vec![
            PaneTab::Pending(PendingPane {
                operation,
                target,
                kind: PaneKind::Terminal,
            }),
            PaneTab::Ready(PendingPane {
                operation: OperationId::new(),
                target,
                kind: PaneKind::Diff,
            }),
            PaneTab::Interrupted(InterruptedPane {
                tab: history,
                resuming: None,
            }),
            PaneTab::Live(LivePane {
                terminal: live.clone(),
                kind: PaneKind::Agent,
            }),
        ];
        assert_eq!(registry.live_terminals(), vec![live]);

        assert!(
            reduce_registry(
                &mut registry,
                PaneRegistryEvent::Pane {
                    target,
                    event: PaneEvent::Select(PaneSelection::None),
                },
            )
            .is_empty()
        );
    }

    #[test]
    #[should_panic(expected = "target-scoped pane restore has an active target")]
    fn target_scoped_restore_rejects_an_impossible_none_selection() {
        let mut state = PaneState::new(PaneSelection::None);
        let _ = reduce(
            &mut state,
            PaneEvent::RestoreBatch {
                panes: Vec::new(),
                selected: None,
                replace_order: true,
            },
        );
    }
}

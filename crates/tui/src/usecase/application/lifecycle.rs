//! Session create/remove の pending row と safe landing を扱う純粋 reducer。
//!
//! daemon wire には依存しない。adapter は [`Effect`] を daemon request に変換し、
//! accepted/progress/final を [`DaemonEvent`] として戻す。各 operation は
//! [`OperationId`] と単調 revision で対応付けるため、遅延・重複 event は UI の
//! 現在地を巻き戻さない。

use std::collections::{HashSet, VecDeque};

use usagi_core::domain::id::{OperationId, SessionId, WorkspaceId};

/// session row に必要な TUI-local projection。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// stable session identity。
    pub id: SessionId,
    /// safe に表示できる label。
    pub label: String,
}

/// navigation target。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// workspace root。
    Root,
    /// session row。
    Session(SessionId),
}

/// navigation cursor。New は action row で active にはならない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// root または session row。
    Target(Target),
    /// `+ new session` action row。
    NewSession,
}

/// Home の常駐表示 mode。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// session list を操作する。
    Switch,
    /// active session の詳細を表示する。
    Closeup,
}

/// pending row の種別と skeleton の復元情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingRow {
    /// 実体が未確定の非選択 create skeleton。
    Creating { label: String },
    /// 既存 row を in-place で置き換える remove skeleton。
    Removing { row: SessionRow },
}

impl PendingRow {
    /// pending row が表す既存 session。create skeleton は identity を持たない。
    #[must_use]
    pub const fn session_id(&self) -> Option<SessionId> {
        match self {
            Self::Creating { .. } => None,
            Self::Removing { row } => Some(row.id),
        }
    }
}

/// operation と pending UI の対応。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOperation {
    /// durable operation identity。
    pub operation_id: OperationId,
    /// skeleton 表示。
    pub row: PendingRow,
    /// 最後に適用した operation revision。accepted は 0。
    pub revision: u64,
    /// accepted 時点の全入力 counter。
    pub interaction_at_accept: u64,
    /// safe progress message。
    pub progress: Option<String>,
}

/// controller が所有する lifecycle projection。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleState {
    workspace: WorkspaceId,
    sessions: Vec<SessionRow>,
    selected: Selection,
    active: Target,
    mode: Mode,
    interaction_count: u64,
    requested: HashSet<OperationId>,
    pending: Vec<PendingOperation>,
    completed: HashSet<OperationId>,
    error: Option<String>,
}

impl LifecycleState {
    /// root を選択した lifecycle projection を作る。
    #[must_use]
    pub fn new(workspace: WorkspaceId, sessions: Vec<SessionRow>) -> Self {
        Self {
            workspace,
            sessions,
            selected: Selection::Target(Target::Root),
            active: Target::Root,
            mode: Mode::Switch,
            interaction_count: 0,
            requested: HashSet::new(),
            pending: Vec::new(),
            completed: HashSet::new(),
            error: None,
        }
    }

    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }
    #[must_use]
    pub fn sessions(&self) -> &[SessionRow] {
        &self.sessions
    }
    #[must_use]
    pub const fn selected(&self) -> Selection {
        self.selected
    }
    #[must_use]
    pub const fn active(&self) -> Target {
        self.active
    }
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }
    #[must_use]
    pub const fn interaction_count(&self) -> u64 {
        self.interaction_count
    }
    #[must_use]
    pub fn pending(&self) -> &[PendingOperation] {
        &self.pending
    }
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// TUI が受ける input。いずれも操作の有無として数える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interaction {
    Key,
    Click,
    Scroll,
    RightClick,
    Other,
}

/// adapter が reducer へ戻す daemon projection。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonEvent {
    /// request が受理された。revision は pending の初期値である 0。
    Accepted {
        operation_id: OperationId,
        row: PendingRow,
    },
    /// safe な progress。revision は operation ごとに単調増加する。
    Progress {
        operation_id: OperationId,
        revision: u64,
        message: String,
    },
    /// create/remove が成功した。create には作成済み row を渡す。
    Succeeded {
        operation_id: OperationId,
        revision: u64,
        created: Option<SessionRow>,
    },
    /// create/remove が失敗した。message は safe error に限る。
    Failed {
        operation_id: OperationId,
        revision: u64,
        message: String,
    },
}

/// reducer input。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// create request。accepted まで skeleton は表示しない。
    RequestCreate {
        operation_id: OperationId,
        label: String,
    },
    /// remove request。accepted まで既存 row は通常表示する。
    RequestRemove {
        operation_id: OperationId,
        session: SessionId,
    },
    /// input は inert なものも含め必ず count する。
    Interaction(Interaction),
    /// daemon lifecycle update。
    Daemon(DaemonEvent),
}

/// reducer が adapter に依頼する外部操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Create {
        workspace: WorkspaceId,
        operation_id: OperationId,
        label: String,
    },
    Remove {
        workspace: WorkspaceId,
        operation_id: OperationId,
        session: SessionId,
    },
}

/// event を state に還元し、必要なら daemon request を返す。
#[must_use]
pub fn update(state: &mut LifecycleState, event: Event) -> Vec<Effect> {
    match event {
        Event::RequestCreate {
            operation_id,
            label,
        } => {
            if state.completed.contains(&operation_id) || !state.requested.insert(operation_id) {
                Vec::new()
            } else {
                vec![Effect::Create {
                    workspace: state.workspace,
                    operation_id,
                    label,
                }]
            }
        }
        Event::RequestRemove {
            operation_id,
            session,
        } => {
            if state.completed.contains(&operation_id)
                || !state.requested.insert(operation_id)
                || !state.sessions.iter().any(|row| row.id == session)
            {
                Vec::new()
            } else {
                vec![Effect::Remove {
                    workspace: state.workspace,
                    operation_id,
                    session,
                }]
            }
        }
        Event::Interaction(_) => {
            state.interaction_count = state.interaction_count.saturating_add(1);
            Vec::new()
        }
        Event::Daemon(event) => apply_daemon(state, event),
    }
}

fn apply_daemon(state: &mut LifecycleState, event: DaemonEvent) -> Vec<Effect> {
    match event {
        DaemonEvent::Accepted { operation_id, row } => accepted(state, operation_id, row),
        DaemonEvent::Progress {
            operation_id,
            revision,
            message,
        } => progress(state, operation_id, revision, message),
        DaemonEvent::Succeeded {
            operation_id,
            revision,
            created,
        } => succeeded(state, operation_id, revision, created),
        DaemonEvent::Failed {
            operation_id,
            revision,
            message,
        } => failed(state, operation_id, revision, message),
    }
}

fn accepted(state: &mut LifecycleState, operation_id: OperationId, row: PendingRow) -> Vec<Effect> {
    if state.completed.contains(&operation_id) || pending(state, operation_id).is_some() {
        return Vec::new();
    }
    if matches!(&row, PendingRow::Removing { row } if !state.sessions.contains(row)) {
        return Vec::new();
    }
    state.pending.push(PendingOperation {
        operation_id,
        row,
        revision: 0,
        interaction_at_accept: state.interaction_count,
        progress: None,
    });
    Vec::new()
}

fn progress(
    state: &mut LifecycleState,
    operation_id: OperationId,
    revision: u64,
    message: String,
) -> Vec<Effect> {
    let Some(operation) = pending_mut(state, operation_id) else {
        return Vec::new();
    };
    if revision <= operation.revision {
        return Vec::new();
    }
    operation.revision = revision;
    operation.progress = Some(message);
    Vec::new()
}

fn succeeded(
    state: &mut LifecycleState,
    operation_id: OperationId,
    revision: u64,
    created: Option<SessionRow>,
) -> Vec<Effect> {
    let Some(index) = pending_index(state, operation_id) else {
        return Vec::new();
    };
    if revision <= state.pending[index].revision
        || !valid_final(&state.pending[index].row, created.as_ref())
    {
        return Vec::new();
    }
    let pending = state.pending.remove(index);
    state.completed.insert(operation_id);
    match pending.row {
        PendingRow::Creating { .. } => land_created(
            state,
            pending.interaction_at_accept,
            &created.expect("validated"),
        ),
        PendingRow::Removing { row } => land_removed(state, pending.interaction_at_accept, &row),
    }
    Vec::new()
}

fn valid_final(row: &PendingRow, created: Option<&SessionRow>) -> bool {
    matches!(
        (row, created),
        (PendingRow::Creating { .. }, Some(_)) | (PendingRow::Removing { .. }, None)
    )
}

fn land_created(state: &mut LifecycleState, accepted_at: u64, row: &SessionRow) {
    if !state.sessions.iter().any(|existing| existing.id == row.id) {
        state.sessions.push(row.clone());
    }
    if state.interaction_count == accepted_at {
        state.selected = Selection::Target(Target::Session(row.id));
        state.active = Target::Session(row.id);
        state.mode = Mode::Closeup;
    }
}

fn land_removed(state: &mut LifecycleState, accepted_at: u64, row: &SessionRow) {
    let landing = state
        .sessions
        .iter()
        .position(|existing| existing.id == row.id)
        .and_then(|position| {
            state
                .sessions
                .get(position + 1)
                .or_else(|| position.checked_sub(1).and_then(|i| state.sessions.get(i)))
        })
        .cloned();
    state.sessions.retain(|existing| existing.id != row.id);
    if state.interaction_count == accepted_at {
        let target = landing.map_or(Target::Root, |next| Target::Session(next.id));
        state.selected = Selection::Target(target);
        state.active = target;
        state.mode = Mode::Switch;
    }
}

fn failed(
    state: &mut LifecycleState,
    operation_id: OperationId,
    revision: u64,
    message: String,
) -> Vec<Effect> {
    let Some(index) = pending_index(state, operation_id) else {
        return Vec::new();
    };
    if revision <= state.pending[index].revision {
        return Vec::new();
    }
    state.pending.remove(index);
    state.completed.insert(operation_id);
    state.error = Some(message);
    Vec::new()
}

fn pending(state: &LifecycleState, operation_id: OperationId) -> Option<&PendingOperation> {
    state
        .pending
        .iter()
        .find(|operation| operation.operation_id == operation_id)
}

fn pending_mut(
    state: &mut LifecycleState,
    operation_id: OperationId,
) -> Option<&mut PendingOperation> {
    state
        .pending
        .iter_mut()
        .find(|operation| operation.operation_id == operation_id)
}

fn pending_index(state: &LifecycleState, operation_id: OperationId) -> Option<usize> {
    state
        .pending
        .iter()
        .position(|operation| operation.operation_id == operation_id)
}

/// reducer scenario 用の fake daemon。IO を持たず request log と event queue だけを持つ。
#[derive(Debug, Default)]
pub struct FakeDaemon {
    effects: Vec<Effect>,
    events: VecDeque<DaemonEvent>,
}

impl FakeDaemon {
    /// dispatch 済み request を確認する。
    #[must_use]
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }
    /// request log を取り出し、空にする。
    #[must_use]
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
    /// daemon event を末尾へ積む。
    pub fn push_event(&mut self, event: DaemonEvent) {
        self.events.push_back(event);
    }
    fn dispatch(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
    fn next_event(&mut self) -> Option<DaemonEvent> {
        self.events.pop_front()
    }
}

/// effects を fake daemon へ送り、queue 済み event を reducer へ戻す。
pub fn run_fake_cycle(state: &mut LifecycleState, daemon: &mut FakeDaemon, effects: Vec<Effect>) {
    for effect in effects {
        daemon.dispatch(effect);
    }
    while let Some(event) = daemon.next_event() {
        let _ = update(state, Event::Daemon(event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> SessionRow {
        SessionRow {
            id: SessionId::new(),
            label: label.to_owned(),
        }
    }
    fn state(rows: Vec<SessionRow>) -> LifecycleState {
        LifecycleState::new(WorkspaceId::new(), rows)
    }

    struct Case {
        name: &'static str,
        initial: Vec<SessionRow>,
        events: Vec<Event>,
        sessions: usize,
        selected: Selection,
        active: Target,
        mode: Mode,
        error: Option<&'static str>,
    }

    fn assert_case(case: Case) {
        let mut state = state(case.initial);
        for event in case.events {
            let _ = update(&mut state, event);
        }
        assert!(state.pending().is_empty(), "{}", case.name);
        assert_eq!(state.sessions().len(), case.sessions, "{}", case.name);
        assert_eq!(state.selected(), case.selected, "{}", case.name);
        assert_eq!(state.active(), case.active, "{}", case.name);
        assert_eq!(state.mode(), case.mode, "{}", case.name);
        assert_eq!(state.error(), case.error, "{}", case.name);
    }

    fn create_success_case() -> Case {
        let operation = OperationId::new();
        let created = row("created");
        Case {
            name: "accepted/progress/final",
            initial: vec![],
            sessions: 1,
            selected: Selection::Target(Target::Session(created.id)),
            active: Target::Session(created.id),
            mode: Mode::Closeup,
            error: None,
            events: vec![
                Event::RequestCreate {
                    operation_id: operation,
                    label: "created".into(),
                },
                Event::Daemon(DaemonEvent::Accepted {
                    operation_id: operation,
                    row: PendingRow::Creating {
                        label: "created".into(),
                    },
                }),
                Event::Daemon(DaemonEvent::Progress {
                    operation_id: operation,
                    revision: 1,
                    message: "creating".into(),
                }),
                Event::Daemon(DaemonEvent::Succeeded {
                    operation_id: operation,
                    revision: 2,
                    created: Some(created),
                }),
            ],
        }
    }

    fn create_failure_case() -> Case {
        let operation = OperationId::new();
        Case {
            name: "create failure rollback",
            initial: vec![],
            sessions: 0,
            selected: Selection::Target(Target::Root),
            active: Target::Root,
            mode: Mode::Switch,
            error: Some("create failed"),
            events: vec![
                Event::Daemon(DaemonEvent::Accepted {
                    operation_id: operation,
                    row: PendingRow::Creating {
                        label: "bad".into(),
                    },
                }),
                Event::Daemon(DaemonEvent::Failed {
                    operation_id: operation,
                    revision: 1,
                    message: "create failed".into(),
                }),
            ],
        }
    }

    fn remove_success_case() -> Case {
        let operation = OperationId::new();
        let existing = row("existing");
        let sibling = row("sibling");
        Case {
            name: "remove adjacent landing",
            initial: vec![existing.clone(), sibling.clone()],
            sessions: 1,
            selected: Selection::Target(Target::Session(sibling.id)),
            active: Target::Session(sibling.id),
            mode: Mode::Switch,
            error: None,
            events: vec![
                Event::Daemon(DaemonEvent::Accepted {
                    operation_id: operation,
                    row: PendingRow::Removing { row: existing },
                }),
                Event::Daemon(DaemonEvent::Succeeded {
                    operation_id: operation,
                    revision: 1,
                    created: None,
                }),
            ],
        }
    }

    fn remove_failure_case() -> Case {
        let operation = OperationId::new();
        let existing = row("existing");
        Case {
            name: "remove failure rollback",
            initial: vec![existing.clone()],
            sessions: 1,
            selected: Selection::Target(Target::Root),
            active: Target::Root,
            mode: Mode::Switch,
            error: Some("remove failed"),
            events: vec![
                Event::Daemon(DaemonEvent::Accepted {
                    operation_id: operation,
                    row: PendingRow::Removing { row: existing },
                }),
                Event::Daemon(DaemonEvent::Failed {
                    operation_id: operation,
                    revision: 1,
                    message: "remove failed".into(),
                }),
            ],
        }
    }

    #[test]
    fn table_driven_fake_daemon_lifecycle_scenarios() {
        for case in [
            create_success_case(),
            create_failure_case(),
            remove_success_case(),
            remove_failure_case(),
        ] {
            assert_case(case);
        }
    }

    #[test]
    fn stale_duplicate_and_unknown_events_are_ignored() {
        let operation = OperationId::new();
        let mut state = state(vec![]);
        let accepted = DaemonEvent::Accepted {
            operation_id: operation,
            row: PendingRow::Creating {
                label: "new".into(),
            },
        };
        let _ = update(&mut state, Event::Daemon(accepted.clone()));
        let _ = update(&mut state, Event::Daemon(accepted));
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Progress {
                operation_id: operation,
                revision: 2,
                message: "newer".into(),
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Progress {
                operation_id: operation,
                revision: 1,
                message: "stale".into(),
            }),
        );
        assert_eq!(state.pending().len(), 1);
        assert_eq!(state.pending()[0].progress.as_deref(), Some("newer"));
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Succeeded {
                operation_id: operation,
                revision: 3,
                created: Some(row("new")),
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Failed {
                operation_id: operation,
                revision: 4,
                message: "late".into(),
            }),
        );
        assert_eq!(state.sessions().len(), 1);
        assert_eq!(state.error(), None);
    }

    #[test]
    fn every_input_cancels_safe_landing_but_not_the_operation() {
        let operation = OperationId::new();
        let created = row("created");
        let mut state = state(vec![]);
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Accepted {
                operation_id: operation,
                row: PendingRow::Creating {
                    label: "created".into(),
                },
            }),
        );
        for input in [
            Interaction::Key,
            Interaction::Click,
            Interaction::Scroll,
            Interaction::RightClick,
            Interaction::Other,
        ] {
            let _ = update(&mut state, Event::Interaction(input));
        }
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Succeeded {
                operation_id: operation,
                revision: 1,
                created: Some(created.clone()),
            }),
        );
        assert_eq!(state.interaction_count(), 5);
        assert_eq!(state.sessions(), &[created]);
        assert_eq!(state.active(), Target::Root);
        assert_eq!(state.mode(), Mode::Switch);
    }

    #[test]
    fn fake_daemon_dispatches_requests_and_drains_events() {
        let mut state = state(vec![]);
        let mut daemon = FakeDaemon::default();
        let operation = OperationId::new();
        let effects = update(
            &mut state,
            Event::RequestCreate {
                operation_id: operation,
                label: "new".into(),
            },
        );
        daemon.push_event(DaemonEvent::Accepted {
            operation_id: operation,
            row: PendingRow::Creating {
                label: "new".into(),
            },
        });
        run_fake_cycle(&mut state, &mut daemon, effects);
        assert_eq!(daemon.effects().len(), 1);
        assert_eq!(state.pending().len(), 1);
    }

    #[test]
    fn requests_dedupe_and_accessors_are_available() {
        let existing = row("existing");
        let mut state = state(vec![existing.clone()]);
        assert_eq!(state.workspace(), state.workspace);
        assert_eq!(
            PendingRow::Creating {
                label: "new".into()
            }
            .session_id(),
            None
        );
        assert_eq!(
            PendingRow::Removing {
                row: existing.clone()
            }
            .session_id(),
            Some(existing.id)
        );

        let create = OperationId::new();
        let first = update(
            &mut state,
            Event::RequestCreate {
                operation_id: create,
                label: "new".into(),
            },
        );
        assert_eq!(first.len(), 1);
        assert!(
            update(
                &mut state,
                Event::RequestCreate {
                    operation_id: create,
                    label: "new".into()
                }
            )
            .is_empty()
        );
        let missing = update(
            &mut state,
            Event::RequestRemove {
                operation_id: OperationId::new(),
                session: SessionId::new(),
            },
        );
        assert!(missing.is_empty());
        let remove = OperationId::new();
        assert!(matches!(
            update(
                &mut state,
                Event::RequestRemove {
                    operation_id: remove,
                    session: existing.id
                }
            )[0],
            Effect::Remove { .. }
        ));

        let mut daemon = FakeDaemon::default();
        run_fake_cycle(&mut state, &mut daemon, first);
        assert_eq!(daemon.take_effects().len(), 1);
        assert!(daemon.effects().is_empty());
    }

    #[test]
    fn invalid_and_late_daemon_events_do_not_mutate_state() {
        let existing = row("existing");
        let mut state = state(vec![existing.clone()]);
        let create = OperationId::new();
        let remove = OperationId::new();
        let unknown = OperationId::new();
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Progress {
                operation_id: unknown,
                revision: 1,
                message: "late".into(),
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Succeeded {
                operation_id: unknown,
                revision: 1,
                created: Some(row("late")),
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Failed {
                operation_id: unknown,
                revision: 1,
                message: "late".into(),
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Accepted {
                operation_id: unknown,
                row: PendingRow::Removing { row: row("gone") },
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Accepted {
                operation_id: create,
                row: PendingRow::Creating {
                    label: "new".into(),
                },
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Succeeded {
                operation_id: create,
                revision: 1,
                created: None,
            }),
        );
        assert_eq!(state.pending().len(), 1);
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Failed {
                operation_id: create,
                revision: 1,
                message: "failed".into(),
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Failed {
                operation_id: create,
                revision: 2,
                message: "late".into(),
            }),
        );
        assert_eq!(state.error(), Some("failed"));

        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Accepted {
                operation_id: remove,
                row: PendingRow::Removing { row: existing },
            }),
        );
        let _ = update(&mut state, Event::Interaction(Interaction::Key));
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Succeeded {
                operation_id: remove,
                revision: 1,
                created: None,
            }),
        );
        assert_eq!(state.mode(), Mode::Switch);
    }

    #[test]
    fn stale_failure_does_not_remove_a_newer_pending_operation() {
        let operation = OperationId::new();
        let mut state = state(vec![]);
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Accepted {
                operation_id: operation,
                row: PendingRow::Creating {
                    label: "new".into(),
                },
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Progress {
                operation_id: operation,
                revision: 2,
                message: "newer".into(),
            }),
        );
        let _ = update(
            &mut state,
            Event::Daemon(DaemonEvent::Failed {
                operation_id: operation,
                revision: 1,
                message: "stale".into(),
            }),
        );
        assert_eq!(state.pending().len(), 1);
    }
}

#![coverage(off)]

//! Closeup の terminal / Agent tab を扱う純粋な reducer。
//!
//! daemon の inventory や stream はここに持ち込まない。adapter は request と completion、
//! exit を [`PaneEvent`] に翻訳し、[`reduce`] が返す [`PaneEffect`] だけを実行する。
//! tab の identity は表示名ではなく、完全な [`TerminalRef`] である。

use usagi_core::domain::id::{OperationId, TerminalRef};

use super::controller::Target;

/// Closeup tab が表示する terminal 種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    /// shell などの通常 terminal。
    Terminal,
    /// terminal 上で起動する Agent。
    Agent,
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

/// Closeup tab。pending は operation、live は terminal incarnation で識別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneTab {
    /// terminal resolve または Agent start の完了待ち。
    Pending(PendingPane),
    /// attach 済みまたは attach 可能な terminal。
    Live(LivePane),
}

/// tab を index でなく stable identity により選ぶための key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabSelection {
    /// completion 前の placeholder。
    Pending(OperationId),
    /// completion 後または保存済みの live tab。
    Live(TerminalRef),
}

/// attach 判断に必要な TUI-local の選択位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneSelection {
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
        PaneEvent::Failed { operation, message } => fail(state, operation, message),
        PaneEvent::Exited(terminal) => exit(state, &terminal),
        PaneEvent::Restore(pane) => restore(state, pane),
    }
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
                PaneTab::Pending(_) | PaneTab::Live(_) => None,
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

fn fail(state: &mut PaneState, operation: OperationId, message: String) -> Vec<PaneEffect> {
    let Some((index, target)) = state
        .tabs
        .iter()
        .enumerate()
        .find_map(|(index, tab)| match tab {
            PaneTab::Pending(pending) if pending.operation == operation => {
                Some((index, pending.target))
            }
            PaneTab::Pending(_) | PaneTab::Live(_) => None,
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

fn selection_for(tab: &PaneTab) -> PaneSelection {
    match tab {
        PaneTab::Pending(pending) => PaneSelection::Tab(TabSelection::Pending(pending.operation)),
        PaneTab::Live(live) => PaneSelection::Tab(TabSelection::Live(live.terminal.clone())),
    }
}

#[cfg(test)]
mod tests {
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
}

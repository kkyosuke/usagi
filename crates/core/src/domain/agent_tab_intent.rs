//! Durable, secret-free display intent for TUI Agent tabs.
//!
//! This state is deliberately not a runtime registry. Daemon inventories remain
//! authoritative for liveness and PTY ownership; this module only reconciles
//! their exact resource fences with the user's saved order, selection, and
//! continuation-scoped dismissals.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::agent::{AgentInventory, AgentRuntimeInventoryState};
use super::id::{AgentContinuationRef, SessionId, TerminalRef, WorkspaceId};
use super::terminal_launch::{TerminalInventoryEntry, TerminalKind};

/// Current on-disk schema understood by the Agent-tab intent store.
pub const AGENT_TAB_INTENT_SCHEMA: u32 = 1;

/// One continuation-backed tab slot in the user's saved order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTabSlotIntent {
    /// Daemon-issued, provider-neutral conversation lineage.
    pub continuation: AgentContinuationRef,
    /// Last exact terminal fence observed for this lineage.
    pub terminal: TerminalRef,
}

/// Ordered Agent slots and saved selection for one root/session target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTabTargetIntent {
    /// Absent for the workspace root, present for a managed session.
    pub session_id: Option<SessionId>,
    pub tabs: Vec<AgentTabSlotIntent>,
    /// Agent selection only. A selected generic terminal is represented by
    /// `None` because generic pane intent is intentionally not persisted here.
    pub selected: Option<AgentContinuationRef>,
}

/// Versioned workspace-scoped TUI display intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTabIntent {
    pub schema: u32,
    pub workspace_id: WorkspaceId,
    /// Monotonic store revision used as a compare-and-swap fence.
    pub revision: u64,
    pub targets: Vec<AgentTabTargetIntent>,
    /// Authoritative user close intent. It is union-preserved across stale
    /// writers until a complete daemon history snapshot proves retention GC or
    /// the user explicitly reopens the lineage.
    pub dismissed: BTreeSet<AgentContinuationRef>,
}

impl AgentTabIntent {
    /// Empty intent used for a workspace with no prior state or ignored legacy
    /// state. No identity is inferred from names or paths.
    #[must_use]
    pub fn empty(workspace_id: WorkspaceId) -> Self {
        Self {
            schema: AGENT_TAB_INTENT_SCHEMA,
            workspace_id,
            revision: 0,
            targets: Vec::new(),
            dismissed: BTreeSet::new(),
        }
    }

    /// A stable display label containing no provider-native metadata.
    #[must_use]
    pub fn safe_label(continuation: AgentContinuationRef) -> String {
        let id = continuation.as_str();
        format!("Agent {}", &id[..8])
    }

    /// Apply one stable-key mutation to the latest state. This is safe to call
    /// after a CAS conflict because it never replaces the whole snapshot.
    pub fn apply(&mut self, mutation: AgentTabIntentMutation) -> Option<AgentTabProjection> {
        match mutation {
            AgentTabIntentMutation::Observe {
                terminals,
                agents,
                allowed_sessions,
            } => Some(self.reconcile(&terminals, &agents, &allowed_sessions)),
            AgentTabIntentMutation::Upsert {
                session_id,
                continuation,
                terminal,
                select,
            } => {
                self.upsert(session_id, continuation, terminal, select);
                None
            }
            AgentTabIntentMutation::Select {
                session_id,
                continuation,
            } => {
                self.target_mut(session_id).selected = continuation;
                None
            }
            AgentTabIntentMutation::Dismiss { continuation } => {
                self.dismissed.insert(continuation);
                self.repair_selections();
                None
            }
            AgentTabIntentMutation::Reopen { continuation } => {
                self.dismissed.remove(&continuation);
                None
            }
            AgentTabIntentMutation::Reorder {
                session_id,
                continuations,
            } => {
                self.reorder(session_id, &continuations);
                None
            }
        }
    }

    /// Project the current state against inventories without changing the
    /// caller's value. Useful after a close/reopen mutation with a cached daemon
    /// snapshot.
    #[must_use]
    pub fn projected(
        &self,
        terminals: &[TerminalInventoryEntry],
        agents: &AgentInventory,
        allowed_sessions: &BTreeSet<SessionId>,
    ) -> AgentTabProjection {
        let mut copy = self.clone();
        copy.reconcile(terminals, agents, allowed_sessions)
    }

    fn target_mut(&mut self, session_id: Option<SessionId>) -> &mut AgentTabTargetIntent {
        if let Some(index) = self
            .targets
            .iter()
            .position(|target| target.session_id == session_id)
        {
            return &mut self.targets[index];
        }
        self.targets.push(AgentTabTargetIntent {
            session_id,
            tabs: Vec::new(),
            selected: None,
        });
        self.targets
            .last_mut()
            .expect("pushing an Agent target leaves one target")
    }

    fn upsert(
        &mut self,
        session_id: Option<SessionId>,
        continuation: AgentContinuationRef,
        terminal: TerminalRef,
        select: bool,
    ) {
        self.dismissed.remove(&continuation);
        let target = self.target_mut(session_id);
        if let Some(slot) = target
            .tabs
            .iter_mut()
            .find(|slot| slot.continuation == continuation)
        {
            slot.terminal = terminal;
        } else {
            target.tabs.push(AgentTabSlotIntent {
                continuation,
                terminal,
            });
        }
        if select {
            target.selected = Some(continuation);
        }
    }

    fn reorder(&mut self, session_id: Option<SessionId>, continuations: &[AgentContinuationRef]) {
        let target = self.target_mut(session_id);
        let mut old = std::mem::take(&mut target.tabs);
        for continuation in continuations {
            if let Some(index) = old
                .iter()
                .position(|slot| slot.continuation == *continuation)
            {
                target.tabs.push(old.remove(index));
            }
        }
        target.tabs.extend(old);
    }

    fn repair_selections(&mut self) {
        for target in &mut self.targets {
            target.selected = repaired_selection(target, &self.dismissed, |_| true);
        }
    }

    fn reconcile(
        &mut self,
        terminals: &[TerminalInventoryEntry],
        agents: &AgentInventory,
        allowed_sessions: &BTreeSet<SessionId>,
    ) -> AgentTabProjection {
        if self.schema != AGENT_TAB_INTENT_SCHEMA || agents.workspace_id != self.workspace_id {
            return AgentTabProjection::default();
        }

        let (live, durable) =
            inventory_sets(self.workspace_id, terminals, agents, allowed_sessions);

        // Only a complete durable-history snapshot makes absence authoritative.
        // Transport failures and partial snapshots therefore cannot collect a
        // dismissal or dormant slot.
        if agents.complete {
            self.dismissed
                .retain(|continuation| durable.contains(continuation));
            for target in &mut self.targets {
                target
                    .tabs
                    .retain(|slot| durable.contains(&slot.continuation));
            }
        }

        self.targets
            .retain(|target| target_allowed(target.session_id, allowed_sessions));
        let mut claimed = BTreeSet::new();
        for target in &mut self.targets {
            target.tabs.retain_mut(|slot| {
                let Some(candidates) = live.get(&slot.continuation) else {
                    return durable.contains(&slot.continuation) || !agents.complete;
                };
                let candidate = candidates
                    .iter()
                    .find(|candidate| candidate.fences(&slot.terminal))
                    .unwrap_or_else(|| &candidates[0]);
                if candidate.session_id != target.session_id || claimed.contains(&slot.continuation)
                {
                    return false;
                }
                slot.terminal = candidate.clone();
                claimed.insert(slot.continuation);
                true
            });
        }

        let mut unclaimed = live
            .iter()
            .filter(|(continuation, _)| !claimed.contains(continuation))
            .filter_map(|(continuation, terminals)| {
                terminals
                    .first()
                    .map(|terminal| (*continuation, terminal.clone()))
            })
            .collect::<Vec<_>>();
        unclaimed.sort_by_key(|(continuation, terminal)| {
            (
                terminal.session_id.map(|id| id.as_str()),
                continuation.as_str(),
                terminal_sort_key(terminal),
            )
        });
        for (continuation, terminal) in unclaimed {
            self.target_mut(terminal.session_id)
                .tabs
                .push(AgentTabSlotIntent {
                    continuation,
                    terminal,
                });
        }

        let mut projection = AgentTabProjection::default();
        for target in &mut self.targets {
            let selected = repaired_selection(target, &self.dismissed, |slot| {
                live.get(&slot.continuation)
                    .is_some_and(|items| items.iter().any(|item| item.fences(&slot.terminal)))
            });
            // A partial snapshot may omit the saved foreground lineage. Use an
            // available fallback for this projection, but do not make that
            // omission authoritative in the persisted display intent.
            if agents.complete || target.selected.is_none() {
                target.selected = selected;
            }
            let tabs = target
                .tabs
                .iter()
                .filter(|slot| !self.dismissed.contains(&slot.continuation))
                .filter(|slot| {
                    live.get(&slot.continuation)
                        .is_some_and(|items| items.iter().any(|item| item.fences(&slot.terminal)))
                })
                .cloned()
                .collect::<Vec<_>>();
            if !tabs.is_empty() {
                projection.targets.push(AgentTabTargetProjection {
                    session_id: target.session_id,
                    tabs,
                    selected,
                });
            }
        }
        projection
    }
}

fn target_allowed(session_id: Option<SessionId>, allowed_sessions: &BTreeSet<SessionId>) -> bool {
    session_id.is_none_or(|id| allowed_sessions.contains(&id))
}

fn inventory_sets(
    workspace: WorkspaceId,
    terminals: &[TerminalInventoryEntry],
    agents: &AgentInventory,
    allowed_sessions: &BTreeSet<SessionId>,
) -> (
    BTreeMap<AgentContinuationRef, Vec<TerminalRef>>,
    BTreeSet<AgentContinuationRef>,
) {
    let trusted_live = |terminal: &TerminalRef| {
        terminal.workspace_id == workspace
            && target_allowed(terminal.session_id, allowed_sessions)
            && terminals.iter().any(|entry| {
                entry.live && entry.kind == TerminalKind::Agent && entry.terminal.fences(terminal)
            })
    };
    let mut live: BTreeMap<AgentContinuationRef, Vec<TerminalRef>> = BTreeMap::new();
    let mut durable = BTreeSet::new();
    for item in &agents.runtimes {
        durable.insert(item.continuation);
        if item.state == AgentRuntimeInventoryState::Live && trusted_live(&item.runtime.terminal) {
            live.entry(item.continuation)
                .or_default()
                .push(item.runtime.terminal.clone());
        }
    }
    for target in agents
        .resumable
        .iter()
        .filter_map(|item| item.target.as_ref())
    {
        durable.insert(target.continuation);
    }
    for candidates in live.values_mut() {
        candidates.sort_by_key(terminal_sort_key);
        candidates.dedup();
    }
    (live, durable)
}

fn repaired_selection(
    target: &AgentTabTargetIntent,
    dismissed: &BTreeSet<AgentContinuationRef>,
    is_available: impl Fn(&AgentTabSlotIntent) -> bool,
) -> Option<AgentContinuationRef> {
    let available =
        |slot: &AgentTabSlotIntent| !dismissed.contains(&slot.continuation) && is_available(slot);
    if let Some(selected) = target.selected {
        if target
            .tabs
            .iter()
            .any(|slot| slot.continuation == selected && available(slot))
        {
            return Some(selected);
        }
        if let Some(index) = target
            .tabs
            .iter()
            .position(|slot| slot.continuation == selected)
        {
            return target.tabs[index..]
                .iter()
                .chain(target.tabs[..index].iter().rev())
                .find(|slot| available(slot))
                .map(|slot| slot.continuation);
        }
    }
    target
        .tabs
        .iter()
        .find(|slot| available(slot))
        .map(|slot| slot.continuation)
}

fn terminal_sort_key(terminal: &TerminalRef) -> (String, String, String, String, String) {
    (
        terminal.daemon_generation.as_str(),
        terminal.terminal_id.as_str(),
        terminal.workspace_id.as_str(),
        terminal
            .session_id
            .map_or_else(String::new, |id| id.as_str()),
        terminal.worktree_id.as_str(),
    )
}

/// A stable-key mutation applied under the store lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentTabIntentMutation {
    /// Reconcile a successful daemon observation. `complete == false` on the
    /// Agent inventory prevents absence-based GC.
    Observe {
        terminals: Vec<TerminalInventoryEntry>,
        agents: AgentInventory,
        allowed_sessions: BTreeSet<SessionId>,
    },
    /// Record a newly admitted or replacement Agent runtime.
    Upsert {
        session_id: Option<SessionId>,
        continuation: AgentContinuationRef,
        terminal: TerminalRef,
        select: bool,
    },
    /// Persist the selected Agent lineage, or `None` when a generic/empty tab is
    /// selected.
    Select {
        session_id: Option<SessionId>,
        continuation: Option<AgentContinuationRef>,
    },
    /// Hide one lineage without stopping its runtime or provider conversation.
    Dismiss { continuation: AgentContinuationRef },
    /// Explicitly clear one dismissal. This does not spawn or resume anything.
    Reopen { continuation: AgentContinuationRef },
    /// Reorder known stable keys while preserving keys omitted by a stale writer.
    Reorder {
        session_id: Option<SessionId>,
        continuations: Vec<AgentContinuationRef>,
    },
}

/// Live Agent tabs to project for one target after reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTabTargetProjection {
    pub session_id: Option<SessionId>,
    pub tabs: Vec<AgentTabSlotIntent>,
    pub selected: Option<AgentContinuationRef>,
}

/// Workspace-wide live projection. Dormant resumable slots remain in
/// [`AgentTabIntent`] for the interrupted-pane owner but are not emitted here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentTabProjection {
    pub targets: Vec<AgentTabTargetProjection>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{
        AgentResumableInventoryItem, AgentResumeTarget, AgentRuntimeInventoryItem,
        ProviderResumeReason,
    };
    use crate::domain::id::{
        AgentResumeSourceId, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, TerminalId,
        WorktreeId,
    };

    fn terminal(
        workspace: WorkspaceId,
        session: Option<SessionId>,
        worktree: WorktreeId,
    ) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: session,
            worktree_id: worktree,
        }
    }

    fn runtime(
        continuation: AgentContinuationRef,
        terminal: &TerminalRef,
        state: AgentRuntimeInventoryState,
    ) -> AgentRuntimeInventoryItem {
        AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef::new(
                AgentRuntimeId::new(),
                terminal.clone(),
                terminal.session_id,
            )
            .unwrap(),
            continuation,
            state,
            resumed_from: None,
        }
    }

    fn live_entry(terminal: TerminalRef) -> TerminalInventoryEntry {
        TerminalInventoryEntry {
            terminal,
            kind: TerminalKind::Agent,
            live: true,
        }
    }

    #[test]
    fn saved_order_selection_and_inventory_only_runtime_reconcile_once() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let discovered = AgentContinuationRef::new();
        let first_terminal = terminal(workspace, Some(session), worktree);
        let second_terminal = terminal(workspace, Some(session), worktree);
        let discovered_terminal = terminal(workspace, Some(session), worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), second, second_terminal.clone(), false);
        intent.upsert(Some(session), first, first_terminal.clone(), true);

        let inventory = AgentInventory {
            workspace_id: workspace,
            complete: true,
            runtimes: vec![
                runtime(first, &first_terminal, AgentRuntimeInventoryState::Live),
                runtime(
                    discovered,
                    &discovered_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
                runtime(second, &second_terminal, AgentRuntimeInventoryState::Live),
            ],
            resumable: Vec::new(),
        };
        let terminals = vec![
            live_entry(discovered_terminal.clone()),
            live_entry(first_terminal.clone()),
            live_entry(second_terminal.clone()),
            live_entry(first_terminal),
        ];
        let projection = intent.reconcile(&terminals, &inventory, &BTreeSet::from([session]));
        assert_eq!(projection.targets.len(), 1);
        let target = &projection.targets[0];
        assert_eq!(
            target
                .tabs
                .iter()
                .map(|slot| slot.continuation)
                .collect::<Vec<_>>(),
            [second, first, discovered]
        );
        assert_eq!(target.selected, Some(first));
        assert_eq!(intent.targets[0].tabs, target.tabs);
    }

    #[test]
    fn dismissal_suppresses_replacement_and_reopen_only_clears_that_lineage() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let closed = AgentContinuationRef::new();
        let other = AgentContinuationRef::new();
        let old = terminal(workspace, Some(session), worktree);
        let replacement = terminal(workspace, Some(session), worktree);
        let other_terminal = terminal(workspace, Some(session), worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), closed, old, true);
        intent.upsert(Some(session), other, other_terminal.clone(), false);
        intent.apply(AgentTabIntentMutation::Dismiss {
            continuation: closed,
        });
        let inventory = AgentInventory {
            workspace_id: workspace,
            complete: true,
            runtimes: vec![
                runtime(closed, &replacement, AgentRuntimeInventoryState::Live),
                runtime(other, &other_terminal, AgentRuntimeInventoryState::Live),
            ],
            resumable: Vec::new(),
        };
        let terminals = vec![live_entry(replacement), live_entry(other_terminal)];
        let first = intent.reconcile(&terminals, &inventory, &BTreeSet::from([session]));
        assert_eq!(first.targets[0].tabs.len(), 1);
        assert_eq!(first.targets[0].tabs[0].continuation, other);
        assert_eq!(first.targets[0].selected, Some(other));

        intent.apply(AgentTabIntentMutation::Reopen {
            continuation: closed,
        });
        let reopened = intent.projected(&terminals, &inventory, &BTreeSet::from([session]));
        assert_eq!(reopened.targets[0].tabs.len(), 2);
        assert!(!intent.dismissed.contains(&closed));
        assert_eq!(AgentTabIntent::safe_label(closed).len(), "Agent ".len() + 8);
    }

    #[test]
    fn resumable_slot_stays_dormant_and_partial_absence_never_collects_intent() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let continuation = AgentContinuationRef::new();
        let old = terminal(workspace, Some(session), worktree);
        let runtime_id = AgentRuntimeId::new();
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), continuation, old.clone(), true);
        intent.dismissed.insert(continuation);
        let target = AgentResumeTarget {
            continuation,
            source: AgentResumeSourceId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
            runtime_id,
            adapter_revision: 1,
        };
        let interrupted = AgentInventory {
            workspace_id: workspace,
            complete: true,
            runtimes: vec![AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef::new(runtime_id, old, Some(session)).unwrap(),
                continuation,
                state: AgentRuntimeInventoryState::Interrupted,
                resumed_from: None,
            }],
            resumable: vec![AgentResumableInventoryItem {
                runtime_id,
                target: Some(target),
                available: true,
                reason: ProviderResumeReason::ExplicitResumeAvailable,
            }],
        };
        assert!(
            intent
                .reconcile(&[], &interrupted, &BTreeSet::from([session]))
                .targets
                .is_empty()
        );
        assert_eq!(intent.targets[0].tabs.len(), 1);
        assert!(intent.dismissed.contains(&continuation));

        let partial = AgentInventory {
            workspace_id: workspace,
            complete: false,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        };
        intent.reconcile(&[], &partial, &BTreeSet::from([session]));
        assert_eq!(intent.targets[0].tabs.len(), 1);
        assert!(intent.dismissed.contains(&continuation));

        let complete = AgentInventory {
            complete: true,
            ..partial
        };
        intent.reconcile(&[], &complete, &BTreeSet::from([session]));
        assert!(intent.targets[0].tabs.is_empty());
        assert!(!intent.dismissed.contains(&continuation));
    }

    #[test]
    fn partial_snapshot_does_not_erase_saved_selection() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let continuation = AgentContinuationRef::new();
        let saved = terminal(workspace, Some(session), worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), continuation, saved, true);

        let partial = AgentInventory {
            workspace_id: workspace,
            complete: false,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        };
        assert!(
            intent
                .reconcile(&[], &partial, &BTreeSet::from([session]))
                .targets
                .is_empty()
        );
        assert_eq!(intent.targets[0].selected, Some(continuation));
    }

    #[test]
    fn exact_trusted_draining_ref_survives_generation_difference_and_bad_scope_is_ignored() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let continuation = AgentContinuationRef::new();
        let draining = terminal(workspace, Some(session), worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), continuation, draining.clone(), true);
        let inventory = AgentInventory {
            workspace_id: workspace,
            complete: true,
            runtimes: vec![runtime(
                continuation,
                &draining,
                AgentRuntimeInventoryState::Live,
            )],
            resumable: Vec::new(),
        };
        assert_eq!(
            intent
                .reconcile(
                    &[live_entry(draining)],
                    &inventory,
                    &BTreeSet::from([session]),
                )
                .targets[0]
                .tabs
                .len(),
            1
        );

        let wrong = AgentInventory {
            workspace_id: WorkspaceId::new(),
            ..inventory
        };
        let before = intent.clone();
        assert!(
            intent
                .reconcile(&[], &wrong, &BTreeSet::from([session]))
                .targets
                .is_empty()
        );
        assert_eq!(intent, before);
    }

    #[test]
    fn stable_mutations_preserve_omitted_tabs_and_do_not_store_provider_material() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: first,
            terminal: terminal(workspace, Some(session), worktree),
            select: false,
        });
        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation: second,
            terminal: terminal(workspace, Some(session), worktree),
            select: true,
        });
        intent.apply(AgentTabIntentMutation::Reorder {
            session_id: Some(session),
            continuations: vec![second],
        });
        intent.apply(AgentTabIntentMutation::Select {
            session_id: Some(session),
            continuation: None,
        });
        assert_eq!(
            intent.targets[0]
                .tabs
                .iter()
                .map(|slot| slot.continuation)
                .collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(intent.targets[0].selected, None);
        let encoded = serde_json::to_string(&intent).unwrap();
        for forbidden in ["provider", "argv", "environment", "transcript", "output"] {
            assert!(!encoded.contains(forbidden));
        }
    }

    #[test]
    fn replacement_duplicate_targets_and_inventory_sorting_are_deterministic() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let saved = AgentContinuationRef::new();
        let replacement = terminal(workspace, Some(session), worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(
            Some(session),
            saved,
            terminal(workspace, Some(session), worktree),
            true,
        );
        intent.upsert(Some(session), saved, replacement.clone(), true);
        let missing_selection = AgentContinuationRef::new();
        intent.targets.insert(
            0,
            AgentTabTargetIntent {
                session_id: None,
                tabs: vec![AgentTabSlotIntent {
                    continuation: saved,
                    terminal: replacement.clone(),
                }],
                selected: Some(missing_selection),
            },
        );

        let root_continuation = AgentContinuationRef::new();
        let root_terminal = terminal(workspace, None, WorktreeId::new());
        let session_continuation = AgentContinuationRef::new();
        let session_terminal = terminal(workspace, Some(session), WorktreeId::new());
        let inventory = AgentInventory {
            workspace_id: workspace,
            complete: false,
            runtimes: vec![
                runtime(saved, &replacement, AgentRuntimeInventoryState::Live),
                runtime(
                    root_continuation,
                    &root_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
                runtime(
                    session_continuation,
                    &session_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
            ],
            resumable: Vec::new(),
        };
        let projection = intent.reconcile(
            &[
                live_entry(replacement),
                live_entry(root_terminal),
                live_entry(session_terminal),
            ],
            &inventory,
            &BTreeSet::from([session]),
        );
        assert_eq!(
            projection
                .targets
                .iter()
                .map(|target| target.tabs.len())
                .sum::<usize>(),
            3
        );
        assert_eq!(intent.targets[0].selected, Some(missing_selection));
    }
}

//! Durable, secret-free display intent for TUI Agent tabs.
//!
//! This state is deliberately not a runtime registry. Daemon inventories remain
//! authoritative for liveness and PTY ownership; this module only reconciles
//! their exact resource fences with the user's saved order, selection, and
//! continuation-scoped dismissals.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use usagi_core::domain::agent::{AgentInventory, AgentRuntimeInventoryState};
use usagi_core::domain::id::{AgentContinuationRef, SessionId, TerminalRef, WorkspaceId};
use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};

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
    /// writers until the user explicitly reopens the lineage.
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

    /// Validate the workspace scope and stable-key uniqueness of persisted
    /// state before it participates in reconciliation.
    ///
    /// # Errors
    ///
    /// Returns a non-sensitive reason when the state cannot be trusted.
    pub fn validate(&self, expected_workspace: WorkspaceId) -> Result<(), IntentValidationError> {
        if self.schema != AGENT_TAB_INTENT_SCHEMA {
            return Err(IntentValidationError::UnsupportedSchema);
        }
        if self.workspace_id != expected_workspace {
            return Err(IntentValidationError::WorkspaceMismatch);
        }
        let mut targets = BTreeSet::new();
        let mut continuations = BTreeSet::new();
        for target in &self.targets {
            if !targets.insert(target.session_id) {
                return Err(IntentValidationError::DuplicateTarget);
            }
            for slot in &target.tabs {
                if slot.terminal.workspace_id != expected_workspace
                    || slot.terminal.session_id != target.session_id
                {
                    return Err(IntentValidationError::ScopeMismatch);
                }
                if !continuations.insert(slot.continuation) {
                    return Err(IntentValidationError::DuplicateContinuation);
                }
            }
            if let Some(selected) = target.selected
                && (self.dismissed.contains(&selected)
                    || !target.tabs.iter().any(|slot| slot.continuation == selected))
            {
                return Err(IntentValidationError::InvalidSelection);
            }
        }
        if !self
            .dismissed
            .iter()
            .all(|continuation| continuations.contains(continuation))
        {
            return Err(IntentValidationError::InvalidDismissal);
        }
        Ok(())
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
                if let Some(target) = self
                    .targets
                    .iter_mut()
                    .find(|target| target.session_id == session_id)
                    && continuation.is_none_or(|candidate| {
                        !self.dismissed.contains(&candidate)
                            && target
                                .tabs
                                .iter()
                                .any(|slot| slot.continuation == candidate)
                    })
                {
                    target.selected = continuation;
                }
                None
            }
            AgentTabIntentMutation::Dismiss { continuation } => {
                if self.targets.iter().any(|target| {
                    target
                        .tabs
                        .iter()
                        .any(|slot| slot.continuation == continuation)
                }) {
                    self.dismissed.insert(continuation);
                    self.repair_selections();
                }
                None
            }
            AgentTabIntentMutation::DismissAndSelect {
                continuation,
                session_id,
                selected,
            } => {
                let _ = self.apply(AgentTabIntentMutation::Dismiss { continuation });
                let _ = self.apply(AgentTabIntentMutation::Select {
                    session_id,
                    continuation: selected,
                });
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

    /// Project a stale observation after a compare-and-swap conflict without
    /// substituting any saved exact terminal fence or discovering a new slot.
    /// A saved lineage absent from the observation is omitted until a fresh
    /// observation can prove its latest [`TerminalRef`].
    #[must_use]
    pub fn projected_exact(
        &self,
        terminals: &[TerminalInventoryEntry],
        agents: &AgentInventory,
        allowed_sessions: &BTreeSet<SessionId>,
    ) -> AgentTabProjection {
        let mut copy = self.clone();
        copy.reconcile_with_policy(terminals, agents, allowed_sessions, false)
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
        let Some(target) = self
            .targets
            .iter_mut()
            .find(|target| target.session_id == session_id)
        else {
            return;
        };
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
            // `None` is a durable statement that a generic (or empty) tab was
            // selected. Closing an unrelated Agent must not manufacture an
            // Agent selection and steal foreground on the next restore.
            if target.selected.is_some() {
                target.selected = repaired_selection(target, &self.dismissed, &|_| true);
            }
        }
    }

    fn reconcile(
        &mut self,
        terminals: &[TerminalInventoryEntry],
        agents: &AgentInventory,
        allowed_sessions: &BTreeSet<SessionId>,
    ) -> AgentTabProjection {
        self.reconcile_with_policy(terminals, agents, allowed_sessions, true)
    }

    fn reconcile_with_policy(
        &mut self,
        terminals: &[TerminalInventoryEntry],
        agents: &AgentInventory,
        allowed_sessions: &BTreeSet<SessionId>,
        may_replace_or_discover: bool,
    ) -> AgentTabProjection {
        if self.schema != AGENT_TAB_INTENT_SCHEMA || agents.workspace_id != self.workspace_id {
            return AgentTabProjection::default();
        }

        let live = inventory_set(self.workspace_id, terminals, agents, allowed_sessions);

        // The successfully refreshed managed-session set is authoritative for
        // scope existence. Remove a vanished target and only dismissals owned by
        // its slots in one mutation, so validation cannot observe orphan keys
        // and unrelated targets keep their close intent.
        let removed = self
            .targets
            .iter()
            .filter(|target| !target_allowed(target.session_id, allowed_sessions))
            .flat_map(|target| target.tabs.iter().map(|slot| slot.continuation))
            .collect::<BTreeSet<_>>();
        self.targets
            .retain(|target| target_allowed(target.session_id, allowed_sessions));
        self.dismissed
            .retain(|continuation| !removed.contains(continuation));
        let mut claimed = BTreeSet::new();
        for target in &mut self.targets {
            target.tabs.retain_mut(|slot| {
                let Some(candidates) = live.get(&slot.continuation) else {
                    // Inventory absence is never a retention/GC signal. A dormant
                    // slot remains durable until an explicit future policy owns
                    // its collection.
                    return true;
                };
                let candidate = candidates
                    .iter()
                    .find(|candidate| candidate.fences(&slot.terminal))
                    .or_else(|| {
                        may_replace_or_discover
                            .then(|| {
                                candidates
                                    .iter()
                                    .find(|candidate| same_attach_domain(&slot.terminal, candidate))
                            })
                            .flatten()
                    });
                let Some(candidate) = candidate else {
                    // On a stale CAS projection the continuation is already
                    // owned by this latest durable slot. Do not rediscover the
                    // stale candidate as a second slot below.
                    claimed.insert(slot.continuation);
                    return true;
                };
                if candidate.session_id != target.session_id || claimed.contains(&slot.continuation)
                {
                    return false;
                }
                slot.terminal = candidate.clone();
                claimed.insert(slot.continuation);
                true
            });
        }

        let mut unclaimed = if may_replace_or_discover {
            live.iter()
                .filter(|(continuation, _)| !claimed.contains(continuation))
                .filter_map(|(continuation, terminals)| {
                    terminals
                        .first()
                        .map(|terminal| (*continuation, terminal.clone()))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
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
            let selected = target.selected.and_then(|_| {
                repaired_selection(target, &self.dismissed, &|slot| {
                    live.get(&slot.continuation)
                        .is_some_and(|items| items.iter().any(|item| item.fences(&slot.terminal)))
                })
            });
            // An observation may omit the saved foreground lineage. Use an
            // available fallback for this projection, but do not make absence
            // authoritative in the persisted display intent.
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

fn same_attach_domain(saved: &TerminalRef, candidate: &TerminalRef) -> bool {
    saved.daemon_generation == candidate.daemon_generation
        && saved.workspace_id == candidate.workspace_id
        && saved.session_id == candidate.session_id
        && saved.worktree_id == candidate.worktree_id
}

/// Safe reason a persisted Agent-tab intent cannot be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentValidationError {
    UnsupportedSchema,
    WorkspaceMismatch,
    DuplicateTarget,
    ScopeMismatch,
    DuplicateContinuation,
    InvalidSelection,
    InvalidDismissal,
}

fn target_allowed(session_id: Option<SessionId>, allowed_sessions: &BTreeSet<SessionId>) -> bool {
    session_id.is_none_or(|id| allowed_sessions.contains(&id))
}

fn inventory_set(
    workspace: WorkspaceId,
    terminals: &[TerminalInventoryEntry],
    agents: &AgentInventory,
    allowed_sessions: &BTreeSet<SessionId>,
) -> BTreeMap<AgentContinuationRef, Vec<TerminalRef>> {
    let trusted_live = |terminal: &TerminalRef| {
        terminal.workspace_id == workspace
            && target_allowed(terminal.session_id, allowed_sessions)
            && terminals.iter().any(|entry| {
                entry.live && entry.kind == TerminalKind::Agent && entry.terminal.fences(terminal)
            })
    };
    let mut live: BTreeMap<AgentContinuationRef, Vec<TerminalRef>> = BTreeMap::new();
    for item in &agents.runtimes {
        if item.state == AgentRuntimeInventoryState::Live && trusted_live(&item.runtime.terminal) {
            live.entry(item.continuation)
                .or_default()
                .push(item.runtime.terminal.clone());
        }
    }
    for candidates in live.values_mut() {
        candidates.sort_by_key(terminal_sort_key);
        candidates.dedup();
    }
    live
}

fn repaired_selection(
    target: &AgentTabTargetIntent,
    dismissed: &BTreeSet<AgentContinuationRef>,
    is_available: &dyn Fn(&AgentTabSlotIntent) -> bool,
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
    /// Reconcile a successful daemon observation. Absence never performs
    /// retention or garbage collection.
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
    /// Atomically hide one lineage and persist the successor chosen by the pane
    /// reducer. `selected=None` means a generic or empty tab owns foreground.
    DismissAndSelect {
        continuation: AgentContinuationRef,
        session_id: Option<SessionId>,
        selected: Option<AgentContinuationRef>,
    },
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

/// Typed, display-safe failure at the TUI display-intent persistence boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTabIntentError {
    /// The private store could not be locked, read, or atomically published.
    Unavailable,
    /// The on-disk schema is newer and must remain untouched.
    ReadOnlySchema,
    /// Another TUI changed the same causal action before it committed.
    ConcurrentChange,
    /// The proposed mutation or revision could not be trusted.
    InvalidMutation,
}

impl AgentTabIntentError {
    /// Secret-free feedback suitable for a TUI notice.
    #[must_use]
    pub const fn safe_message(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "Agent tab state could not be saved; visible tabs were left unchanged"
            }
            Self::ReadOnlySchema => {
                "Agent tab state belongs to a newer usagi; visible tabs were left unchanged"
            }
            Self::ConcurrentChange => "Agent tabs changed in another TUI; retry the action",
            Self::InvalidMutation => {
                "Agent tab change was rejected; visible tabs were left unchanged"
            }
        }
    }
}

/// Workspace-scoped persistence boundary for the TUI-only display intent.
/// Implementations apply stable-key mutations under a file lock and return the
/// merged latest revision after a compare-and-swap conflict.
pub trait AgentTabIntentPort: Send {
    /// Load the latest valid display intent, treating missing or safely ignored
    /// legacy state as empty.
    ///
    /// # Errors
    ///
    /// Returns display-safe persistence feedback.
    fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError>;

    /// Atomically merge one stable-key mutation into the latest revision.
    ///
    /// # Errors
    ///
    /// Returns display-safe lock, read, serialization, or publish feedback.
    fn mutate(
        &mut self,
        workspace: WorkspaceId,
        expected_revision: u64,
        mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError>;
}

/// Result of one locked persistence mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTabIntentPortCommit {
    pub intent: AgentTabIntent,
    pub projection: Option<AgentTabProjection>,
    /// Whether the requested mutation was causally accepted. A stale Observe
    /// or anti-monotonic Reopen may return the latest safe projection/state but
    /// still require a fresh retry.
    pub mutation_applied: bool,
    /// The caller observed an older revision; its stable-key delta was merged
    /// into the latest state instead of replacing it.
    pub cas_conflict: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::agent::{
        AgentResumableInventoryItem, AgentResumeTarget, AgentRuntimeInventoryItem,
        ProviderResumeReason,
    };
    use usagi_core::domain::id::{
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
        let mut replacement = old.clone();
        replacement.terminal_id = TerminalId::new();
        let other_terminal = terminal(workspace, Some(session), worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), closed, old, true);
        intent.upsert(Some(session), other, other_terminal.clone(), false);
        intent.apply(AgentTabIntentMutation::Dismiss {
            continuation: closed,
        });
        let inventory = AgentInventory {
            workspace_id: workspace,
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
    fn upsert_never_reopens_an_explicitly_dismissed_lineage() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let continuation = AgentContinuationRef::new();
        let old = terminal(workspace, Some(session), worktree);
        let mut replacement = old.clone();
        replacement.terminal_id = TerminalId::new();
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), continuation, old, true);
        intent.apply(AgentTabIntentMutation::Dismiss { continuation });

        intent.apply(AgentTabIntentMutation::Upsert {
            session_id: Some(session),
            continuation,
            terminal: replacement.clone(),
            select: true,
        });

        assert!(intent.dismissed.contains(&continuation));
        assert!(intent.targets[0].tabs[0].terminal.fences(&replacement));
        let inventory = AgentInventory {
            workspace_id: workspace,
            runtimes: vec![runtime(
                continuation,
                &replacement,
                AgentRuntimeInventoryState::Live,
            )],
            resumable: Vec::new(),
        };
        assert!(
            intent
                .projected(
                    &[live_entry(replacement)],
                    &inventory,
                    &BTreeSet::from([session]),
                )
                .targets
                .is_empty()
        );
        intent.apply(AgentTabIntentMutation::Reopen { continuation });
        assert!(!intent.dismissed.contains(&continuation));
    }

    #[test]
    fn resumable_slot_and_inventory_absence_never_collect_intent() {
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
                provider: Some(usagi_core::domain::agent::ProviderKind::Claude),
                last_known_phase: None,
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

        let absent = AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        };
        intent.reconcile(&[], &absent, &BTreeSet::from([session]));
        assert_eq!(intent.targets[0].tabs.len(), 1);
        assert!(intent.dismissed.contains(&continuation));
    }

    #[test]
    fn inventory_absence_does_not_erase_saved_selection() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let continuation = AgentContinuationRef::new();
        let saved = terminal(workspace, Some(session), worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(Some(session), continuation, saved, true);

        let absent = AgentInventory {
            workspace_id: workspace,
            runtimes: Vec::new(),
            resumable: Vec::new(),
        };
        assert!(
            intent
                .reconcile(&[], &absent, &BTreeSet::from([session]))
                .targets
                .is_empty()
        );
        assert_eq!(intent.targets[0].selected, Some(continuation));
    }

    #[test]
    fn generic_selection_stays_none_and_generation_mismatch_never_replaces_saved_ref() {
        let workspace = WorkspaceId::new();
        let worktree = WorktreeId::new();
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let saved = terminal(workspace, None, worktree);
        let other = terminal(workspace, None, worktree);
        let mut wrong_generation = saved.clone();
        wrong_generation.daemon_generation = DaemonGeneration::new();
        wrong_generation.terminal_id = TerminalId::new();
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(None, first, saved.clone(), false);
        intent.upsert(None, second, other.clone(), false);

        let projection = intent.reconcile(
            &[
                live_entry(wrong_generation.clone()),
                live_entry(other.clone()),
            ],
            &AgentInventory {
                workspace_id: workspace,
                runtimes: vec![
                    runtime(first, &wrong_generation, AgentRuntimeInventoryState::Live),
                    runtime(second, &other, AgentRuntimeInventoryState::Live),
                ],
                resumable: Vec::new(),
            },
            &BTreeSet::new(),
        );

        assert_eq!(intent.targets[0].selected, None);
        assert!(intent.targets[0].tabs[0].terminal.fences(&saved));
        assert_eq!(projection.targets[0].selected, None);
        assert_eq!(projection.targets[0].tabs.len(), 1);
        assert_eq!(projection.targets[0].tabs[0].continuation, second);

        let _ = intent.apply(AgentTabIntentMutation::Dismiss {
            continuation: second,
        });
        assert_eq!(intent.targets[0].selected, None);
    }

    #[test]
    fn reconciliation_drops_a_duplicate_claim_and_repairs_an_unknown_selection() {
        let workspace = WorkspaceId::new();
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let fallback = AgentContinuationRef::new();
        let first_terminal = terminal(workspace, Some(first_session), WorktreeId::new());
        let second_terminal = terminal(workspace, Some(second_session), WorktreeId::new());
        let fallback_terminal = terminal(workspace, Some(second_session), WorktreeId::new());
        let mut intent = AgentTabIntent::empty(workspace);
        intent.targets = vec![
            AgentTabTargetIntent {
                session_id: Some(first_session),
                tabs: vec![AgentTabSlotIntent {
                    continuation,
                    terminal: first_terminal.clone(),
                }],
                selected: Some(continuation),
            },
            AgentTabTargetIntent {
                session_id: Some(second_session),
                tabs: vec![
                    AgentTabSlotIntent {
                        continuation,
                        terminal: second_terminal.clone(),
                    },
                    AgentTabSlotIntent {
                        continuation: fallback,
                        terminal: fallback_terminal.clone(),
                    },
                ],
                selected: Some(AgentContinuationRef::new()),
            },
        ];
        let inventory = AgentInventory {
            workspace_id: workspace,
            runtimes: vec![
                runtime(
                    continuation,
                    &first_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
                runtime(
                    continuation,
                    &second_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
                runtime(
                    fallback,
                    &fallback_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
            ],
            resumable: Vec::new(),
        };

        let projection = intent.reconcile(
            &[
                live_entry(first_terminal),
                live_entry(second_terminal),
                live_entry(fallback_terminal),
            ],
            &inventory,
            &BTreeSet::from([first_session, second_session]),
        );

        assert_eq!(intent.targets[0].tabs.len(), 1);
        assert_eq!(intent.targets[1].tabs.len(), 1);
        assert_eq!(intent.targets[1].tabs[0].continuation, fallback);
        assert_eq!(projection.targets[1].selected, Some(fallback));
    }

    #[test]
    fn removed_session_drops_only_its_slots_and_dismissals_atomically() {
        let workspace = WorkspaceId::new();
        let removed_session = SessionId::new();
        let root_worktree = WorktreeId::new();
        let session_worktree = WorktreeId::new();
        let root_live = AgentContinuationRef::new();
        let root_dismissed = AgentContinuationRef::new();
        let session_live = AgentContinuationRef::new();
        let session_dismissed = AgentContinuationRef::new();
        let root_live_terminal = terminal(workspace, None, root_worktree);
        let root_dismissed_terminal = terminal(workspace, None, root_worktree);
        let removed_live_terminal = terminal(workspace, Some(removed_session), session_worktree);
        let removed_dismissed_terminal =
            terminal(workspace, Some(removed_session), session_worktree);
        let mut intent = AgentTabIntent::empty(workspace);
        intent.upsert(None, root_live, root_live_terminal.clone(), true);
        intent.upsert(None, root_dismissed, root_dismissed_terminal.clone(), false);
        intent.upsert(
            Some(removed_session),
            session_live,
            removed_live_terminal,
            true,
        );
        intent.upsert(
            Some(removed_session),
            session_dismissed,
            removed_dismissed_terminal,
            false,
        );
        intent.apply(AgentTabIntentMutation::Dismiss {
            continuation: root_dismissed,
        });
        intent.apply(AgentTabIntentMutation::Dismiss {
            continuation: session_dismissed,
        });
        let removed = intent
            .targets
            .iter()
            .find(|target| target.session_id == Some(removed_session))
            .unwrap();
        assert_eq!(removed.selected, Some(session_live));
        assert!(intent.dismissed.contains(&session_dismissed));

        let inventory = AgentInventory {
            workspace_id: workspace,
            runtimes: vec![
                runtime(
                    root_live,
                    &root_live_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
                runtime(
                    root_dismissed,
                    &root_dismissed_terminal,
                    AgentRuntimeInventoryState::Live,
                ),
            ],
            resumable: Vec::new(),
        };
        let projection = intent.reconcile(
            &[
                live_entry(root_live_terminal),
                live_entry(root_dismissed_terminal),
            ],
            &inventory,
            &BTreeSet::new(),
        );

        assert_eq!(intent.targets.len(), 1);
        assert_eq!(intent.targets[0].session_id, None);
        assert_eq!(intent.dismissed, BTreeSet::from([root_dismissed]));
        assert_eq!(projection.targets.len(), 1);
        assert_eq!(projection.targets[0].selected, Some(root_live));
        assert_eq!(projection.targets[0].tabs.len(), 1);
        assert_eq!(projection.targets[0].tabs[0].continuation, root_live);
        assert_eq!(intent.validate(workspace), Ok(()));
    }

    #[test]
    fn reorder_of_unknown_target_does_not_create_an_empty_target() {
        let workspace = WorkspaceId::new();
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Reorder {
            session_id: Some(SessionId::new()),
            continuations: vec![AgentContinuationRef::new()],
        });
        assert!(intent.targets.is_empty());
        assert_eq!(intent.validate(workspace), Ok(()));
    }

    #[test]
    fn repaired_selection_uses_first_available_tab_when_no_selection_is_saved() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let target = AgentTabTargetIntent {
            session_id: None,
            tabs: vec![AgentTabSlotIntent {
                continuation,
                terminal: terminal(workspace, None, WorktreeId::new()),
            }],
            selected: None,
        };

        assert_eq!(
            repaired_selection(&target, &BTreeSet::new(), &|_| true),
            Some(continuation)
        );
    }

    #[test]
    fn unknown_selection_and_dismissal_are_inert_and_errors_are_safe() {
        let workspace = WorkspaceId::new();
        let mut intent = AgentTabIntent::empty(workspace);
        intent.apply(AgentTabIntentMutation::Select {
            session_id: Some(SessionId::new()),
            continuation: Some(AgentContinuationRef::new()),
        });
        intent.apply(AgentTabIntentMutation::Dismiss {
            continuation: AgentContinuationRef::new(),
        });
        assert!(intent.targets.is_empty());
        assert!(intent.dismissed.is_empty());
        assert_eq!(intent.validate(workspace), Ok(()));

        let workspace_text = workspace.as_str();
        for error in [
            AgentTabIntentError::Unavailable,
            AgentTabIntentError::ReadOnlySchema,
            AgentTabIntentError::ConcurrentChange,
            AgentTabIntentError::InvalidMutation,
        ] {
            assert!(!error.safe_message().is_empty());
            assert!(!error.safe_message().contains(&workspace_text));
        }
    }

    #[test]
    fn validation_rejects_every_persisted_shape_violation() {
        let workspace = WorkspaceId::new();
        let continuation = AgentContinuationRef::new();
        let terminal = terminal(workspace, None, WorktreeId::new());
        let mut valid = AgentTabIntent::empty(workspace);
        valid.upsert(None, continuation, terminal.clone(), true);

        let mut unsupported = valid.clone();
        unsupported.schema += 1;
        assert_eq!(
            unsupported.validate(workspace),
            Err(IntentValidationError::UnsupportedSchema)
        );
        assert_eq!(
            valid.validate(WorkspaceId::new()),
            Err(IntentValidationError::WorkspaceMismatch)
        );

        let mut duplicate_target = valid.clone();
        duplicate_target.targets.push(valid.targets[0].clone());
        assert_eq!(
            duplicate_target.validate(workspace),
            Err(IntentValidationError::DuplicateTarget)
        );

        let mut bad_scope = valid.clone();
        bad_scope.targets[0].tabs[0].terminal.session_id = Some(SessionId::new());
        assert_eq!(
            bad_scope.validate(workspace),
            Err(IntentValidationError::ScopeMismatch)
        );

        let mut duplicate_continuation = valid.clone();
        let mut duplicate_terminal = terminal;
        duplicate_terminal.terminal_id = TerminalId::new();
        duplicate_continuation.targets[0]
            .tabs
            .push(AgentTabSlotIntent {
                continuation,
                terminal: duplicate_terminal,
            });
        assert_eq!(
            duplicate_continuation.validate(workspace),
            Err(IntentValidationError::DuplicateContinuation)
        );

        let mut invalid_selection = valid.clone();
        invalid_selection.targets[0].selected = Some(AgentContinuationRef::new());
        assert_eq!(
            invalid_selection.validate(workspace),
            Err(IntentValidationError::InvalidSelection)
        );
        let mut invalid_dismissal = valid;
        invalid_dismissal
            .dismissed
            .insert(AgentContinuationRef::new());
        assert_eq!(
            invalid_dismissal.validate(workspace),
            Err(IntentValidationError::InvalidDismissal)
        );
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
            continuations: vec![second, AgentContinuationRef::new()],
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
}

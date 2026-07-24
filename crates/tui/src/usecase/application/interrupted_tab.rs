//! Interrupted Agent tabs and their explicit, per-tab provider resume (#510).
//!
//! After a daemon crash, `SIGKILL`, or cold stop/start the old PTY is gone. The
//! daemon still retains each conversation lineage as an **interrupted** runtime
//! plus its exact resume source, and publishes both through `AgentInventory`.
//! This pure reducer decides which of those lineages the TUI shows as its own
//! tab, and it validates every step of one explicit resume so a stale, ambiguous,
//! or replayed answer can never attach the wrong runtime.
//!
//! Rules, mirroring the [3. TUI](../../../../document/03-tui.md) and
//! [4. IPC](../../../../document/04-ipc.md) contracts:
//!
//! * An interrupted tab is **never** presented as live and never carries a
//!   subscription, input, or resize affordance. `last_terminal` is display and
//!   ordering material only.
//! * Nothing here starts a provider. [`project`] is a pure read; only
//!   [`resume_command`], called from an explicit user action, produces a request.
//! * A lineage a live (or reserved) runtime still holds is not an interrupted
//!   tab, so a live runtime and its interrupted source converge to one tab.
//! * Root and managed-session lineages, and several histories inside one scope,
//!   are separate tabs keyed by [`AgentContinuationRef`]. Names, provider kind,
//!   and worktrees never merge two lineages.
//! * A lineage without a trustworthy exact target stays visible but
//!   unresumable, carrying only its non-sensitive reason.
//! * Display data is limited to the closed provider/reason vocabulary. Provider
//!   IDs, argv, cwd, and transcripts never enter a tab.

use std::collections::{BTreeMap, BTreeSet};

use usagi_core::domain::agent::{
    AgentInventory, AgentResumableInventoryItem, AgentResumeRelation, AgentResumeTarget,
    AgentRuntimeInventoryItem, AgentRuntimeInventoryState, ProviderKind, ProviderResumePhase,
    ProviderResumeReason,
};
use usagi_core::domain::id::{
    AgentContinuationRef, OperationId, SessionId, TerminalRef, WorkspaceId,
};

/// One interrupted Agent conversation projected as its own tab.
///
/// The tab is read-only until the user explicitly resumes it. It holds no
/// provider-native identity: [`Self::target`] is the daemon-issued opaque source
/// and every other field is closed display vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedTab {
    /// Daemon-issued, provider-neutral conversation lineage. This is the tab's
    /// stable key across reopen, refresh, and duplicate inventory rows.
    pub continuation: AgentContinuationRef,
    /// Owning managed session; absent for a workspace-root Agent.
    pub session_id: Option<SessionId>,
    /// Last known terminal incarnation of the interrupted runtime. It orders the
    /// tab and identifies the dead PTY that must never be attached again.
    pub last_terminal: TerminalRef,
    /// Provider owning the conversation, when metadata named one.
    pub provider: Option<ProviderKind>,
    /// Last durable non-sensitive phase, when one was retained.
    pub last_known_phase: Option<ProviderResumePhase>,
    /// Non-sensitive explanation of the tab's resume availability.
    pub reason: ProviderResumeReason,
    /// Exact resume source. `None` keeps the tab visible but unresumable.
    pub target: Option<AgentResumeTarget>,
}

impl InterruptedTab {
    /// Whether an explicit resume may be requested for this exact tab.
    #[must_use]
    pub const fn resumable(&self) -> bool {
        self.target.is_some()
    }

    /// A stable tab label built only from the closed provider vocabulary.
    #[must_use]
    pub fn safe_label(&self) -> String {
        format!("{} (interrupted)", provider_label(self.provider))
    }

    /// Non-sensitive detail line for the selected tab's body.
    #[must_use]
    pub const fn safe_detail(&self) -> &'static str {
        if self.target.is_some() {
            "This conversation was interrupted. Resume starts a new Agent for it."
        } else {
            reason_detail(self.reason)
        }
    }
}

/// Display name of a provider, or the neutral name when none was retained.
#[must_use]
pub const fn provider_label(provider: Option<ProviderKind>) -> &'static str {
    match provider {
        Some(ProviderKind::Claude) => "Claude",
        Some(ProviderKind::Codex) => "Codex",
        None => "Agent",
    }
}

/// Why an interrupted tab cannot be resumed, in non-sensitive display copy.
#[must_use]
pub const fn reason_detail(reason: ProviderResumeReason) -> &'static str {
    match reason {
        ProviderResumeReason::ExplicitResumeAvailable => {
            "This conversation was interrupted. Resume starts a new Agent for it."
        }
        ProviderResumeReason::LiveOrOwnershipUnknown => {
            "This conversation is still held by a running Agent; it cannot be resumed."
        }
        ProviderResumeReason::ProviderMetadataUnavailable => {
            "This conversation kept no resume metadata; it cannot be resumed."
        }
        ProviderResumeReason::AmbiguousProviderMetadata => {
            "This conversation's resume metadata is ambiguous; it cannot be resumed."
        }
        ProviderResumeReason::IncompatibleProviderMetadata => {
            "This conversation's resume metadata does not match this Agent; it cannot be resumed."
        }
        ProviderResumeReason::SourceAlreadySuperseded => {
            "This conversation was already resumed elsewhere; it cannot be resumed again."
        }
    }
}

/// The interrupted tabs of one workspace, in display order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterruptedProjection {
    /// Deduped by lineage, ordered by the saved display intent first.
    pub tabs: Vec<InterruptedTab>,
}

/// Projects `inventory` into the interrupted tabs one TUI displays.
///
/// * `workspace` fences the observation: an inventory for another workspace
///   projects nothing.
/// * `allowed_sessions` is the set of managed sessions this TUI refreshed
///   successfully. The workspace root is always in scope.
/// * `order` is the saved [`super::agent_tab_intent`] slot order, so a restored
///   tab keeps the position the user last saw it in.
/// * `dismissed` are the lineages the user closed; they stay hidden until they
///   also appear in `reopened`. Inventory absence never clears a dismissal.
///
/// The result is a pure function of its inputs, so a refreshed or duplicated
/// observation converges to the same tabs instead of adding a second one.
#[must_use]
pub fn project(
    inventory: &AgentInventory,
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
    order: &[AgentContinuationRef],
    dismissed: &BTreeSet<AgentContinuationRef>,
    reopened: &BTreeSet<AgentContinuationRef>,
) -> InterruptedProjection {
    if inventory.workspace_id != workspace {
        return InterruptedProjection::default();
    }
    let in_scope = |item: &AgentRuntimeInventoryItem| {
        item.runtime.terminal.workspace_id == workspace
            // A deserialized runtime may disagree with itself; the pane scope is
            // only trustworthy when the runtime and its terminal agree.
            && item.runtime.session_id == item.runtime.terminal.session_id
            && item
                .runtime
                .session_id
                .is_none_or(|session| allowed_sessions.contains(&session))
    };
    let held = inventory
        .runtimes
        .iter()
        .filter(|item| in_scope(item))
        .filter(|item| {
            matches!(
                item.state,
                AgentRuntimeInventoryState::Live | AgentRuntimeInventoryState::Reserved
            )
        })
        .map(|item| item.continuation)
        .collect::<BTreeSet<_>>();
    let sources = inventory
        .resumable
        .iter()
        .map(|item| (item.runtime_id, item))
        .collect::<BTreeMap<_, _>>();

    let mut candidates: BTreeMap<AgentContinuationRef, InterruptedTab> = BTreeMap::new();
    for item in inventory.runtimes.iter().filter(|item| in_scope(item)) {
        if item.state != AgentRuntimeInventoryState::Interrupted
            || held.contains(&item.continuation)
            || (dismissed.contains(&item.continuation) && !reopened.contains(&item.continuation))
        {
            continue;
        }
        let source = sources.get(&item.runtime.agent_runtime_id).copied();
        let tab = tab(item, source);
        // Two interrupted records may share one lineage after a resume that was
        // itself interrupted. Keep the resumable one, then the newest exact ref,
        // so the choice is deterministic instead of inventory-order dependent.
        match candidates.get(&item.continuation) {
            Some(current) if !supersedes(&tab, current) => {}
            _ => {
                candidates.insert(item.continuation, tab);
            }
        }
    }

    let position = order
        .iter()
        .enumerate()
        .map(|(index, continuation)| (*continuation, index))
        .collect::<BTreeMap<_, _>>();
    let mut tabs = candidates.into_values().collect::<Vec<_>>();
    tabs.sort_by_key(|tab| {
        (
            // Saved slots keep their order; a lineage rediscovered from
            // inventory alone follows them deterministically.
            position
                .get(&tab.continuation)
                .copied()
                .unwrap_or(usize::MAX),
            tab.session_id.map(|session| session.as_str()),
            terminal_sort_key(&tab.last_terminal),
            tab.continuation.as_str(),
        )
    });
    InterruptedProjection { tabs }
}

fn tab(
    item: &AgentRuntimeInventoryItem,
    source: Option<&AgentResumableInventoryItem>,
) -> InterruptedTab {
    let Some(source) = source else {
        return InterruptedTab {
            continuation: item.continuation,
            session_id: item.runtime.session_id,
            last_terminal: item.runtime.terminal.clone(),
            provider: None,
            last_known_phase: None,
            reason: ProviderResumeReason::ProviderMetadataUnavailable,
            target: None,
        };
    };
    InterruptedTab {
        continuation: item.continuation,
        session_id: item.runtime.session_id,
        last_terminal: item.runtime.terminal.clone(),
        provider: source.provider,
        last_known_phase: source.last_known_phase,
        reason: source.reason,
        target: trusted_target(item, source),
    }
}

/// Accepts the daemon's exact target only when it describes this very runtime.
/// Anything else is treated as absent, so an unavailable or mismatched source
/// can never seed a resume request. The source row is already keyed by this
/// runtime's ID, so only the target's own fences are re-checked here.
fn trusted_target(
    item: &AgentRuntimeInventoryItem,
    source: &AgentResumableInventoryItem,
) -> Option<AgentResumeTarget> {
    let target = source.target.as_ref()?;
    (source.available
        && source.reason == ProviderResumeReason::ExplicitResumeAvailable
        && target.continuation == item.continuation
        && target.runtime_id == item.runtime.agent_runtime_id
        && target.workspace_id == item.runtime.terminal.workspace_id
        && target.session_id == item.runtime.session_id
        && target.worktree_id == item.runtime.terminal.worktree_id)
        .then(|| target.clone())
}

fn supersedes(candidate: &InterruptedTab, current: &InterruptedTab) -> bool {
    (
        candidate.target.is_some(),
        terminal_sort_key(&candidate.last_terminal),
    ) > (
        current.target.is_some(),
        terminal_sort_key(&current.last_terminal),
    )
}

fn terminal_sort_key(terminal: &TerminalRef) -> (String, String) {
    (
        terminal.daemon_generation.as_str(),
        terminal.terminal_id.as_str(),
    )
}

/// One explicit resume request for exactly one interrupted tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCommand {
    /// The daemon-issued opaque source the user selected.
    pub target: AgentResumeTarget,
    /// Durable operation identity that makes a replayed answer harmless.
    pub operation: OperationId,
}

/// The exact replacement one accepted resume produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeReplacement {
    /// Lineage shared by the interrupted source and its replacement.
    pub continuation: AgentContinuationRef,
    /// New, fully fenced terminal of the replacement runtime.
    pub terminal: TerminalRef,
}

/// Why a resume request or answer was refused. Every variant is safe to show:
/// none of them carries provider metadata or a raw daemon error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeRejection {
    /// The tab has no trustworthy exact target.
    NotResumable,
    /// A resume for this tab is already in flight (a repeated activation).
    AlreadyResuming,
    /// The answer belongs to a different operation.
    OperationMismatch,
    /// The daemon did not return the source-to-replacement relation.
    RelationMissing,
    /// The answer names a different lineage.
    ContinuationMismatch,
    /// The answer names a different interrupted source or runtime.
    SourceMismatch,
    /// The replacement is not a new terminal in this tab's own scope.
    ReplacementNotFenced,
}

impl ResumeRejection {
    /// Secret-free feedback suitable for the pane footer.
    #[must_use]
    pub const fn safe_message(self) -> &'static str {
        match self {
            Self::NotResumable => "This conversation cannot be resumed",
            Self::AlreadyResuming => "This conversation is already being resumed",
            Self::OperationMismatch => "A stale resume answer was ignored",
            Self::RelationMissing => "The daemon did not confirm which conversation was resumed",
            Self::ContinuationMismatch | Self::SourceMismatch => {
                "The daemon answered for a different conversation; refresh and retry"
            }
            Self::ReplacementNotFenced => {
                "The daemon did not return a new Agent terminal; refresh and retry"
            }
        }
    }

    /// Whether the same explicit action is worth repeating after this refusal.
    #[must_use]
    pub const fn retryable(self) -> bool {
        match self {
            Self::NotResumable | Self::AlreadyResuming => false,
            Self::OperationMismatch
            | Self::RelationMissing
            | Self::ContinuationMismatch
            | Self::SourceMismatch
            | Self::ReplacementNotFenced => true,
        }
    }
}

/// Builds the request for one explicitly selected interrupted tab.
///
/// `in_flight` is the operation already resuming this tab, if any: a repeated
/// activation converges to the first operation rather than spawning a second
/// runtime.
///
/// # Errors
///
/// Returns [`ResumeRejection::NotResumable`] when the tab has no trustworthy
/// target and [`ResumeRejection::AlreadyResuming`] for a repeated activation.
pub fn resume_command(
    tab: &InterruptedTab,
    in_flight: Option<OperationId>,
    operation: OperationId,
) -> Result<ResumeCommand, ResumeRejection> {
    if in_flight.is_some() {
        return Err(ResumeRejection::AlreadyResuming);
    }
    tab.target
        .clone()
        .map(|target| ResumeCommand { target, operation })
        .ok_or(ResumeRejection::NotResumable)
}

/// Validates one resume answer before the interrupted tab becomes live.
///
/// The replacement is accepted only when the answered operation, the lineage,
/// the exact interrupted source, and a genuinely new terminal in this tab's own
/// scope all agree. Any disagreement leaves the interrupted tab in place.
///
/// # Errors
///
/// Returns the matching [`ResumeRejection`] for a stale operation, a missing
/// relation, a different lineage or source, or a replacement that is not a new
/// fully fenced terminal of this scope.
pub fn accept_replacement(
    tab: &InterruptedTab,
    in_flight: OperationId,
    answered: OperationId,
    continuation: Option<AgentContinuationRef>,
    relation: Option<&AgentResumeRelation>,
    terminal: &TerminalRef,
) -> Result<ResumeReplacement, ResumeRejection> {
    if answered != in_flight {
        return Err(ResumeRejection::OperationMismatch);
    }
    let target = tab.target.as_ref().ok_or(ResumeRejection::NotResumable)?;
    let relation = relation.ok_or(ResumeRejection::RelationMissing)?;
    if continuation != Some(tab.continuation) {
        return Err(ResumeRejection::ContinuationMismatch);
    }
    if relation.source != target.source || relation.replacement_runtime == target.runtime_id {
        return Err(ResumeRejection::SourceMismatch);
    }
    // The replacement must be one new incarnation of this exact scope. The dead
    // PTY of the interrupted runtime is never a valid answer.
    if !relation.replacement_terminal.fences(terminal)
        || terminal.fences(&tab.last_terminal)
        || terminal.workspace_id != target.workspace_id
        || terminal.session_id != target.session_id
        || terminal.worktree_id != target.worktree_id
    {
        return Err(ResumeRejection::ReplacementNotFenced);
    }
    Ok(ResumeReplacement {
        continuation: tab.continuation,
        terminal: terminal.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::id::{
        AgentResumeSourceId, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, TerminalId,
        WorktreeId,
    };

    struct Scope {
        workspace: WorkspaceId,
        session: Option<SessionId>,
        worktree: WorktreeId,
    }

    impl Scope {
        fn root(workspace: WorkspaceId) -> Self {
            Self {
                workspace,
                session: None,
                worktree: WorktreeId::new(),
            }
        }

        fn session(workspace: WorkspaceId) -> Self {
            Self {
                workspace,
                session: Some(SessionId::new()),
                worktree: WorktreeId::new(),
            }
        }

        fn terminal(&self) -> TerminalRef {
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: self.workspace,
                session_id: self.session,
                worktree_id: self.worktree,
            }
        }
    }

    /// One durable lineage with its interrupted runtime and exact source.
    struct Lineage {
        continuation: AgentContinuationRef,
        runtime_id: AgentRuntimeId,
        source: AgentResumeSourceId,
        terminal: TerminalRef,
        session: Option<SessionId>,
        provider: ProviderKind,
    }

    impl Lineage {
        fn new(scope: &Scope, provider: ProviderKind) -> Self {
            Self {
                continuation: AgentContinuationRef::new(),
                runtime_id: AgentRuntimeId::new(),
                source: AgentResumeSourceId::new(),
                terminal: scope.terminal(),
                session: scope.session,
                provider,
            }
        }

        fn runtime(&self, state: AgentRuntimeInventoryState) -> AgentRuntimeInventoryItem {
            AgentRuntimeInventoryItem {
                runtime: AgentRuntimeRef {
                    agent_runtime_id: self.runtime_id,
                    terminal: self.terminal.clone(),
                    session_id: self.session,
                },
                continuation: self.continuation,
                state,
                resumed_from: None,
            }
        }

        fn target(&self) -> AgentResumeTarget {
            AgentResumeTarget {
                continuation: self.continuation,
                source: self.source,
                workspace_id: self.terminal.workspace_id,
                session_id: self.session,
                worktree_id: self.terminal.worktree_id,
                runtime_id: self.runtime_id,
                adapter_revision: 3,
            }
        }

        fn available(&self) -> AgentResumableInventoryItem {
            AgentResumableInventoryItem {
                runtime_id: self.runtime_id,
                target: Some(self.target()),
                available: true,
                reason: ProviderResumeReason::ExplicitResumeAvailable,
                provider: Some(self.provider),
                last_known_phase: Some(ProviderResumePhase::Interrupted),
            }
        }

        fn unavailable(&self, reason: ProviderResumeReason) -> AgentResumableInventoryItem {
            AgentResumableInventoryItem {
                runtime_id: self.runtime_id,
                target: Some(self.target()),
                available: false,
                reason,
                provider: Some(self.provider),
                last_known_phase: None,
            }
        }

        /// A replacement runtime for this lineage in the same scope.
        fn replacement(&self) -> (AgentResumeRelation, TerminalRef) {
            let terminal = TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: self.terminal.workspace_id,
                session_id: self.session,
                worktree_id: self.terminal.worktree_id,
            };
            (
                AgentResumeRelation {
                    source: self.source,
                    replacement_runtime: AgentRuntimeId::new(),
                    replacement_terminal: terminal.clone(),
                },
                terminal,
            )
        }
    }

    fn inventory(
        workspace: WorkspaceId,
        runtimes: Vec<AgentRuntimeInventoryItem>,
        resumable: Vec<AgentResumableInventoryItem>,
    ) -> AgentInventory {
        AgentInventory {
            workspace_id: workspace,
            runtimes,
            resumable,
        }
    }

    fn projected(inventory: &AgentInventory, workspace: WorkspaceId) -> InterruptedProjection {
        let allowed = inventory
            .runtimes
            .iter()
            .filter_map(|item| item.runtime.session_id)
            .collect();
        project(
            inventory,
            workspace,
            &allowed,
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
    }

    #[test]
    fn root_and_session_histories_become_separate_resumable_tabs() {
        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let managed = Scope::session(workspace);
        let claude = Lineage::new(&root, ProviderKind::Claude);
        let codex = Lineage::new(&managed, ProviderKind::Codex);
        let inventory = inventory(
            workspace,
            vec![
                claude.runtime(AgentRuntimeInventoryState::Interrupted),
                codex.runtime(AgentRuntimeInventoryState::Interrupted),
            ],
            vec![claude.available(), codex.available()],
        );

        let projection = projected(&inventory, workspace);
        assert_eq!(projection.tabs.len(), 2);
        let labels = projection
            .tabs
            .iter()
            .map(InterruptedTab::safe_label)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            labels,
            BTreeSet::from([
                "Claude (interrupted)".to_owned(),
                "Codex (interrupted)".to_owned()
            ])
        );
        assert!(projection.tabs.iter().all(InterruptedTab::resumable));
        // The workspace root is a first-class tab, not a rejected scope.
        assert!(projection.tabs.iter().any(|tab| tab.session_id.is_none()));
        assert!(projection.tabs.iter().any(|tab| tab.session_id.is_some()));
        assert_eq!(
            projection.tabs[0].last_known_phase,
            Some(ProviderResumePhase::Interrupted)
        );
    }

    #[test]
    fn two_histories_in_one_session_stay_two_tabs_and_a_refresh_adds_none() {
        let workspace = WorkspaceId::new();
        let managed = Scope::session(workspace);
        let first = Lineage::new(&managed, ProviderKind::Claude);
        let second = Lineage::new(&managed, ProviderKind::Claude);
        let inventory = inventory(
            workspace,
            vec![
                first.runtime(AgentRuntimeInventoryState::Interrupted),
                second.runtime(AgentRuntimeInventoryState::Interrupted),
                // A duplicated inventory row for the same lineage.
                first.runtime(AgentRuntimeInventoryState::Interrupted),
            ],
            vec![first.available(), second.available()],
        );

        let projection = projected(&inventory, workspace);
        assert_eq!(projection.tabs.len(), 2);
        assert_eq!(
            projection
                .tabs
                .iter()
                .map(|tab| tab.continuation)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([first.continuation, second.continuation])
        );
        // Re-projecting the same observation is idempotent.
        assert_eq!(projected(&inventory, workspace), projection);
    }

    #[test]
    fn saved_order_positions_tabs_and_rediscovered_lineages_follow_deterministically() {
        let workspace = WorkspaceId::new();
        let managed = Scope::session(workspace);
        let first = Lineage::new(&managed, ProviderKind::Claude);
        let second = Lineage::new(&managed, ProviderKind::Codex);
        let inventory = inventory(
            workspace,
            vec![
                first.runtime(AgentRuntimeInventoryState::Interrupted),
                second.runtime(AgentRuntimeInventoryState::Interrupted),
            ],
            vec![first.available(), second.available()],
        );
        let allowed = BTreeSet::from([managed.session.unwrap()]);

        let saved = project(
            &inventory,
            workspace,
            &allowed,
            &[second.continuation, first.continuation],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert_eq!(
            saved
                .tabs
                .iter()
                .map(|tab| tab.continuation)
                .collect::<Vec<_>>(),
            vec![second.continuation, first.continuation]
        );

        // A lineage the saved intent does not know follows the saved ones, and
        // the order stays stable across observations.
        let partial = project(
            &inventory,
            workspace,
            &allowed,
            &[second.continuation],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert_eq!(partial.tabs[0].continuation, second.continuation);
        assert_eq!(partial.tabs[1].continuation, first.continuation);
        assert_eq!(partial, {
            let mut reversed = inventory.clone();
            reversed.runtimes.reverse();
            project(
                &reversed,
                workspace,
                &allowed,
                &[second.continuation],
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
        });
    }

    #[test]
    fn a_live_or_reserved_lineage_is_not_an_interrupted_tab() {
        let workspace = WorkspaceId::new();
        let managed = Scope::session(workspace);
        let lineage = Lineage::new(&managed, ProviderKind::Claude);
        let replacement = AgentRuntimeInventoryItem {
            runtime: AgentRuntimeRef {
                agent_runtime_id: AgentRuntimeId::new(),
                terminal: managed.terminal(),
                session_id: managed.session,
            },
            continuation: lineage.continuation,
            state: AgentRuntimeInventoryState::Live,
            resumed_from: Some(lineage.source),
        };
        let live = inventory(
            workspace,
            vec![
                lineage.runtime(AgentRuntimeInventoryState::Interrupted),
                replacement.clone(),
            ],
            vec![lineage.available()],
        );
        // The live runtime and its interrupted source converge to one live tab.
        assert!(projected(&live, workspace).tabs.is_empty());

        let mut reserved = live;
        reserved.runtimes[1].state = AgentRuntimeInventoryState::Reserved;
        assert!(projected(&reserved, workspace).tabs.is_empty());
    }

    #[test]
    fn exited_reclaimed_and_out_of_scope_runtimes_are_not_interrupted_tabs() {
        let workspace = WorkspaceId::new();
        let managed = Scope::session(workspace);
        let exited = Lineage::new(&managed, ProviderKind::Claude);
        let reclaimed = Lineage::new(&managed, ProviderKind::Codex);
        let inventory = inventory(
            workspace,
            vec![
                exited.runtime(AgentRuntimeInventoryState::Exited),
                reclaimed.runtime(AgentRuntimeInventoryState::Reclaimed),
            ],
            vec![exited.available(), reclaimed.available()],
        );
        // Completed history is owned by the read-only completed-tab projection.
        assert!(projected(&inventory, workspace).tabs.is_empty());

        // A session this TUI did not refresh is out of scope.
        let interrupted = self::inventory(
            workspace,
            vec![exited.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![exited.available()],
        );
        assert!(
            project(
                &interrupted,
                workspace,
                &BTreeSet::new(),
                &[],
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .tabs
            .is_empty()
        );
        // So is an inventory for another workspace.
        assert!(projected(&interrupted, WorkspaceId::new()).tabs.is_empty());
    }

    #[test]
    fn a_runtime_disagreeing_with_its_own_terminal_scope_is_rejected() {
        let workspace = WorkspaceId::new();
        let managed = Scope::session(workspace);
        let lineage = Lineage::new(&managed, ProviderKind::Claude);
        let mut item = lineage.runtime(AgentRuntimeInventoryState::Interrupted);
        item.runtime.session_id = None;
        let inventory = inventory(workspace, vec![item], vec![lineage.available()]);
        assert!(
            project(
                &inventory,
                workspace,
                &BTreeSet::from([managed.session.unwrap()]),
                &[],
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
            .tabs
            .is_empty()
        );
    }

    #[test]
    fn a_dismissed_lineage_stays_hidden_until_an_explicit_reopen() {
        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let lineage = Lineage::new(&root, ProviderKind::Codex);
        let inventory = inventory(
            workspace,
            vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![lineage.available()],
        );
        let dismissed = BTreeSet::from([lineage.continuation]);

        assert!(
            project(
                &inventory,
                workspace,
                &BTreeSet::new(),
                &[],
                &dismissed,
                &BTreeSet::new(),
            )
            .tabs
            .is_empty()
        );
        let reopened = project(
            &inventory,
            workspace,
            &BTreeSet::new(),
            &[],
            &dismissed,
            &BTreeSet::from([lineage.continuation]),
        );
        assert_eq!(reopened.tabs.len(), 1);
        assert!(reopened.tabs[0].resumable());
    }

    #[test]
    fn unavailable_and_untrustworthy_sources_stay_visible_but_unresumable() {
        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let missing = Lineage::new(&root, ProviderKind::Codex);
        let unavailable = Lineage::new(&root, ProviderKind::Codex);
        let inventory = inventory(
            workspace,
            vec![
                missing.runtime(AgentRuntimeInventoryState::Interrupted),
                unavailable.runtime(AgentRuntimeInventoryState::Interrupted),
            ],
            // `missing` has no source row at all: an unavailable Codex capture.
            vec![unavailable.unavailable(ProviderResumeReason::ProviderMetadataUnavailable)],
        );

        let projection = projected(&inventory, workspace);
        assert_eq!(projection.tabs.len(), 2);
        assert!(projection.tabs.iter().all(|tab| !tab.resumable()));
        assert!(
            projection
                .tabs
                .iter()
                .all(|tab| tab.reason == ProviderResumeReason::ProviderMetadataUnavailable)
        );
        assert!(
            projection
                .tabs
                .iter()
                .all(|tab| tab.safe_detail().contains("kept no resume metadata"))
        );
        // A tab without provider metadata still gets a neutral, safe label.
        assert_eq!(
            projection
                .tabs
                .iter()
                .find(|tab| tab.continuation == missing.continuation)
                .map(InterruptedTab::safe_label),
            Some("Agent (interrupted)".to_owned())
        );
    }

    /// One untrustworthy shape of a resumable inventory row.
    type SourceMutation = Box<dyn Fn(&mut AgentResumableInventoryItem)>;

    #[test]
    fn a_target_that_does_not_describe_its_own_runtime_is_dropped() {
        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let lineage = Lineage::new(&root, ProviderKind::Claude);
        let other = Lineage::new(&Scope::session(workspace), ProviderKind::Claude);
        let mutations: Vec<SourceMutation> = vec![
            // Wrong lineage, runtime, scope, and availability all fail closed.
            Box::new(|item| {
                item.target.as_mut().unwrap().continuation = AgentContinuationRef::new();
            }),
            Box::new(|item| {
                item.target.as_mut().unwrap().runtime_id = AgentRuntimeId::new();
            }),
            Box::new(|item| {
                item.target.as_mut().unwrap().workspace_id = WorkspaceId::new();
            }),
            Box::new(|item| {
                item.target.as_mut().unwrap().session_id = Some(SessionId::new());
            }),
            Box::new(|item| {
                item.target.as_mut().unwrap().worktree_id = WorktreeId::new();
            }),
            Box::new(|item| item.available = false),
            Box::new(|item| item.reason = ProviderResumeReason::LiveOrOwnershipUnknown),
            Box::new(|item| item.target = None),
            Box::new(|item| item.runtime_id = AgentRuntimeId::new()),
        ];
        for mutate in mutations {
            let mut source = lineage.available();
            mutate(&mut source);
            let inventory = inventory(
                workspace,
                vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
                vec![source],
            );
            let projection = projected(&inventory, workspace);
            assert_eq!(projection.tabs.len(), 1);
            assert!(!projection.tabs[0].resumable());
        }
        // An unrelated lineage's source row never seeds this tab either.
        let inventory = inventory(
            workspace,
            vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![other.available()],
        );
        assert!(!projected(&inventory, workspace).tabs[0].resumable());
    }

    #[test]
    fn a_lineage_with_two_interrupted_records_keeps_the_resumable_one() {
        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let source = Lineage::new(&root, ProviderKind::Claude);
        let mut retry = Lineage::new(&root, ProviderKind::Claude);
        retry.continuation = source.continuation;
        let inventory = inventory(
            workspace,
            vec![
                source.runtime(AgentRuntimeInventoryState::Interrupted),
                retry.runtime(AgentRuntimeInventoryState::Interrupted),
            ],
            // The older record was superseded; only the retry may be resumed.
            vec![
                source.unavailable(ProviderResumeReason::SourceAlreadySuperseded),
                retry.available(),
            ],
        );

        let projection = projected(&inventory, workspace);
        assert_eq!(projection.tabs.len(), 1);
        assert!(projection.tabs[0].resumable());
        assert_eq!(
            projection.tabs[0]
                .target
                .as_ref()
                .map(|target| target.source),
            Some(retry.source)
        );
    }

    #[test]
    fn resume_is_requested_only_by_an_explicit_action_and_only_once() {
        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let lineage = Lineage::new(&root, ProviderKind::Claude);
        let inventory = inventory(
            workspace,
            vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![lineage.available()],
        );
        let tab = projected(&inventory, workspace).tabs.remove(0);

        let operation = OperationId::new();
        let command = resume_command(&tab, None, operation).unwrap();
        assert_eq!(command.target, lineage.target());
        assert_eq!(command.operation, operation);
        // A repeated activation converges on the operation already in flight.
        assert_eq!(
            resume_command(&tab, Some(operation), OperationId::new()),
            Err(ResumeRejection::AlreadyResuming)
        );
        assert!(!ResumeRejection::AlreadyResuming.retryable());

        let unresumable = InterruptedTab {
            target: None,
            ..tab
        };
        assert_eq!(
            resume_command(&unresumable, None, OperationId::new()),
            Err(ResumeRejection::NotResumable)
        );
        assert!(!ResumeRejection::NotResumable.retryable());
    }

    #[test]
    fn one_confirmed_replacement_replaces_exactly_the_resumed_tab() {
        let workspace = WorkspaceId::new();
        let managed = Scope::session(workspace);
        let lineage = Lineage::new(&managed, ProviderKind::Codex);
        let inventory = inventory(
            workspace,
            vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![lineage.available()],
        );
        let tab = projected(&inventory, workspace).tabs.remove(0);
        let operation = OperationId::new();
        let (relation, terminal) = lineage.replacement();

        let replacement = accept_replacement(
            &tab,
            operation,
            operation,
            Some(lineage.continuation),
            Some(&relation),
            &terminal,
        )
        .unwrap();
        assert_eq!(replacement.continuation, lineage.continuation);
        assert!(replacement.terminal.fences(&terminal));
        // The dead PTY is never the answer.
        assert!(!replacement.terminal.fences(&lineage.terminal));
    }

    #[test]
    fn a_stale_mismatched_or_unconfirmed_answer_keeps_the_interrupted_tab() {
        let workspace = WorkspaceId::new();
        let managed = Scope::session(workspace);
        let lineage = Lineage::new(&managed, ProviderKind::Codex);
        let inventory = inventory(
            workspace,
            vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![lineage.available()],
        );
        let tab = projected(&inventory, workspace).tabs.remove(0);
        let operation = OperationId::new();
        let (relation, terminal) = lineage.replacement();
        let accept = |continuation, relation: Option<&AgentResumeRelation>, terminal| {
            accept_replacement(&tab, operation, operation, continuation, relation, terminal)
        };

        // A late answer for another operation.
        assert_eq!(
            accept_replacement(
                &tab,
                operation,
                OperationId::new(),
                Some(lineage.continuation),
                Some(&relation),
                &terminal,
            ),
            Err(ResumeRejection::OperationMismatch)
        );
        // An ordinary launch reply carries no relation.
        assert_eq!(
            accept(Some(lineage.continuation), None, &terminal),
            Err(ResumeRejection::RelationMissing)
        );
        // A different (or absent) lineage.
        assert_eq!(
            accept(
                Some(AgentContinuationRef::new()),
                Some(&relation),
                &terminal
            ),
            Err(ResumeRejection::ContinuationMismatch)
        );
        assert_eq!(
            accept(None, Some(&relation), &terminal),
            Err(ResumeRejection::ContinuationMismatch)
        );
        // A different interrupted source, or a replacement that reuses the
        // interrupted runtime instead of creating a new one.
        let mut foreign = relation.clone();
        foreign.source = AgentResumeSourceId::new();
        assert_eq!(
            accept(Some(lineage.continuation), Some(&foreign), &terminal),
            Err(ResumeRejection::SourceMismatch)
        );
        let mut reused = relation.clone();
        reused.replacement_runtime = lineage.runtime_id;
        assert_eq!(
            accept(Some(lineage.continuation), Some(&reused), &terminal),
            Err(ResumeRejection::SourceMismatch)
        );
        // The relation and the returned terminal must be the same exact ref.
        let unrelated = managed.terminal();
        assert_eq!(
            accept(Some(lineage.continuation), Some(&relation), &unrelated),
            Err(ResumeRejection::ReplacementNotFenced)
        );
        // Re-announcing the dead PTY, or a replacement in another scope.
        let mut old_pty = relation.clone();
        old_pty.replacement_terminal = lineage.terminal.clone();
        assert_eq!(
            accept(
                Some(lineage.continuation),
                Some(&old_pty),
                &lineage.terminal
            ),
            Err(ResumeRejection::ReplacementNotFenced)
        );
        let mut foreign_scope = relation;
        foreign_scope.replacement_terminal.worktree_id = WorktreeId::new();
        assert_eq!(
            accept(
                Some(lineage.continuation),
                Some(&foreign_scope),
                &foreign_scope.replacement_terminal.clone()
            ),
            Err(ResumeRejection::ReplacementNotFenced)
        );
        assert!(ResumeRejection::ReplacementNotFenced.retryable());
    }

    #[test]
    fn an_answer_for_a_tab_without_a_target_is_refused() {
        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let lineage = Lineage::new(&root, ProviderKind::Claude);
        let inventory = inventory(
            workspace,
            vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![lineage.unavailable(ProviderResumeReason::ProviderMetadataUnavailable)],
        );
        let tab = projected(&inventory, workspace).tabs.remove(0);
        let operation = OperationId::new();
        let (relation, terminal) = lineage.replacement();
        assert_eq!(
            accept_replacement(
                &tab,
                operation,
                operation,
                Some(lineage.continuation),
                Some(&relation),
                &terminal,
            ),
            Err(ResumeRejection::NotResumable)
        );
    }

    #[test]
    fn every_display_string_is_closed_vocabulary_and_carries_no_provider_metadata() {
        let reasons = [
            ProviderResumeReason::ExplicitResumeAvailable,
            ProviderResumeReason::LiveOrOwnershipUnknown,
            ProviderResumeReason::ProviderMetadataUnavailable,
            ProviderResumeReason::AmbiguousProviderMetadata,
            ProviderResumeReason::IncompatibleProviderMetadata,
            ProviderResumeReason::SourceAlreadySuperseded,
        ];
        assert!(
            reasons
                .iter()
                .all(|reason| !reason_detail(*reason).is_empty())
        );
        assert_eq!(provider_label(Some(ProviderKind::Claude)), "Claude");
        assert_eq!(provider_label(Some(ProviderKind::Codex)), "Codex");
        assert_eq!(provider_label(None), "Agent");

        let workspace = WorkspaceId::new();
        let root = Scope::root(workspace);
        let lineage = Lineage::new(&root, ProviderKind::Claude);
        let inventory = inventory(
            workspace,
            vec![lineage.runtime(AgentRuntimeInventoryState::Interrupted)],
            vec![lineage.available()],
        );
        let tab = projected(&inventory, workspace).tabs.remove(0);
        assert!(tab.safe_detail().contains("Resume starts a new Agent"));

        let rejections = [
            ResumeRejection::NotResumable,
            ResumeRejection::AlreadyResuming,
            ResumeRejection::OperationMismatch,
            ResumeRejection::RelationMissing,
            ResumeRejection::ContinuationMismatch,
            ResumeRejection::SourceMismatch,
            ResumeRejection::ReplacementNotFenced,
        ];
        assert!(
            rejections
                .iter()
                .all(|rejection| !rejection.safe_message().is_empty())
        );
        assert_eq!(
            rejections
                .iter()
                .filter(|rejection| rejection.retryable())
                .count(),
            5
        );
        // The derives used by the pane owner stay exercised without leaking any
        // provider-native value into a rendered string.
        let debug = format!("{tab:?} {:?}", InterruptedProjection::default());
        assert!(!debug.is_empty());
        assert_eq!(tab.clone(), tab);
        assert_eq!(
            InterruptedProjection::default(),
            InterruptedProjection::default()
        );
        let command = resume_command(&tab, None, OperationId::new()).unwrap();
        assert_eq!(command.clone(), command);
        assert!(!format!("{command:?}").is_empty());
        let (relation, terminal) = lineage.replacement();
        let replacement = accept_replacement(
            &tab,
            command.operation,
            command.operation,
            Some(lineage.continuation),
            Some(&relation),
            &terminal,
        )
        .unwrap();
        assert_eq!(replacement.clone(), replacement);
        assert!(!format!("{replacement:?}").is_empty());
    }
}

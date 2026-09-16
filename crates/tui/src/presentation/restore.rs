//! 終了した pane / terminal の復元 job と、その対象選定・再試行。

#[cfg(test)]
use super::Geometry;
use super::{
    AgentCommandPort, AgentContinuationRef, AgentInventory, AgentRuntimeInventoryState,
    AgentTabProjection, AppEvent, AppKey, BTreeMap, BTreeSet, InterruptedTab, PaneKind,
    PaneRestoreTarget, Path, RESTORE_RETRY_BASE, RESTORE_RETRY_MAX, RestoreApply,
    RestoreCompletion, RestoreFollowup, RestoreJobOutcome, Sender, SessionId, Target,
    TerminalError, TerminalInventoryEntry, TerminalKind, TerminalRef, WorkspaceDeck, WorkspaceId,
    WorkspaceIoRuntime, WorkspaceLoader, WorkspaceRuntime,
};

/// Controller-owned admission and backoff for the dedicated restore client.
/// Frame ticks only consult this clock; they never imply a reconnect or issue an
/// inventory RPC by themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RestoreRetryState {
    pub(super) in_flight: bool,
    pub(super) followup: RestoreFollowup,
    pub(super) failures: u32,
    pub(super) next_retry_at: Option<std::time::Duration>,
    pub(super) notice_emitted: bool,
    pub(super) last_reconnect_epoch: u64,
}

impl RestoreRetryState {
    pub(super) fn new() -> Self {
        Self {
            in_flight: false,
            followup: RestoreFollowup::None,
            failures: 0,
            next_retry_at: Some(std::time::Duration::ZERO),
            notice_emitted: false,
            last_reconnect_epoch: 0,
        }
    }

    pub(super) fn begin_if_due(&mut self, now: std::time::Duration) -> bool {
        if self.in_flight || self.next_retry_at.is_none_or(|due| now < due) {
            return false;
        }
        self.in_flight = true;
        self.next_retry_at = None;
        true
    }

    /// Request one coherent observation after a durable local mutation. An
    /// existing outage keeps its backoff and an in-flight observation already
    /// sees the daemon state needed by this display-only mutation.
    pub(super) fn request_observation(&mut self, now: std::time::Duration) {
        if !self.in_flight && self.next_retry_at.is_none() {
            self.next_retry_at = Some(now);
        }
    }

    /// Request a snapshot after daemon inventory changed. A snapshot already in
    /// flight may predate that change, so remember one coalesced follow-up.
    pub(super) fn request_changed_observation(&mut self, now: std::time::Duration) {
        if self.in_flight {
            if self.followup == RestoreFollowup::None {
                self.followup = RestoreFollowup::ChangedObservation;
            }
        } else if self.next_retry_at.is_none() {
            self.next_retry_at = Some(now);
        }
    }

    /// Complete one bounded worker job. Returns whether this outage epoch needs
    /// its one coalesced user notice.
    pub(super) fn complete(
        &mut self,
        now: std::time::Duration,
        outcome: RestoreJobOutcome,
    ) -> bool {
        self.in_flight = false;
        let followup = std::mem::replace(&mut self.followup, RestoreFollowup::None);
        if followup == RestoreFollowup::Reconnected {
            self.failures = 0;
            self.next_retry_at = Some(now);
            self.notice_emitted = false;
            return false;
        }
        match outcome {
            RestoreJobOutcome::Applied | RestoreJobOutcome::IntentFailed(_) => {
                self.failures = 0;
                self.next_retry_at =
                    (followup == RestoreFollowup::ChangedObservation).then_some(now);
                self.notice_emitted = false;
                return false;
            }
            // The inventory was observed under an obsolete interaction/revision
            // fence. Its dedicated port is already back, so immediately admit
            // one observation under the fresh fence. This is a UI race, not a
            // daemon outage: do not back off or emit an outage notice.
            RestoreJobOutcome::FenceRejected => {
                self.failures = 0;
                self.next_retry_at = Some(now);
                self.notice_emitted = false;
                return false;
            }
            RestoreJobOutcome::TransportFailed => {}
        }
        self.failures = self.failures.saturating_add(1);
        let shift = self.failures.saturating_sub(1).min(4);
        let delay = RESTORE_RETRY_BASE
            .checked_mul(1_u32 << shift)
            .unwrap_or(RESTORE_RETRY_MAX)
            .min(RESTORE_RETRY_MAX);
        self.next_retry_at = Some(now.saturating_add(delay));
        if self.notice_emitted {
            false
        } else {
            self.notice_emitted = true;
            true
        }
    }

    /// A typed connection-epoch transition schedules exactly one fresh
    /// observation. A transition racing an in-flight job is remembered until
    /// that job returns its dedicated port.
    pub(super) fn reconnected(&mut self, epoch: u64, now: std::time::Duration) {
        if epoch <= self.last_reconnect_epoch {
            return;
        }
        self.last_reconnect_epoch = epoch;
        self.failures = 0;
        self.notice_emitted = false;
        if self.in_flight {
            self.followup = RestoreFollowup::Reconnected;
        } else {
            self.next_retry_at = Some(now);
        }
    }
}

/// Run restore over a dedicated daemon port. Inventory is retried with bounded
/// backoff on this worker, so the first frame and terminal input loop never wait
/// for a handshake or a slow daemon response.
pub(super) fn spawn_restore_job(
    mut port: Box<dyn AgentCommandPort>,
    workspace: WorkspaceId,
    allowed_sessions: BTreeSet<SessionId>,
    dispatched_interaction: u64,
    dispatched_registry_revision: u64,
    sender: Sender<RestoreCompletion>,
) {
    std::thread::spawn(move || {
        let mut terminals = Err(TerminalError::Unavailable);
        let mut agents = Err("Agent inventory is unavailable".to_owned());
        let mut observation_coherent = false;
        for attempt in 0..3 {
            // Bracket the Agent inventory with terminal snapshots. Equal
            // canonical snapshots plus a bijective live-Agent relationship are
            // the optimistic consistency fence available without expanding the
            // IPC protocol in #506.
            let before = port.list_terminals();
            let agent_attempt = port.resume_inventory(workspace).and_then(|inventory| {
                if inventory.workspace_id == workspace {
                    Ok(inventory)
                } else {
                    Err("Agent inventory scope changed while restoring".to_owned())
                }
            });
            let after = port.list_terminals();
            match (before, agent_attempt, after) {
                (Ok(mut before), Ok(inventory), Ok(mut after)) => {
                    normalize_terminal_inventory(&mut before);
                    normalize_terminal_inventory(&mut after);
                    observation_coherent = before == after
                        && restore_inventory_is_coherent(
                            workspace,
                            &allowed_sessions,
                            &after,
                            &inventory,
                        );
                    terminals = Ok(after);
                    agents = Ok(inventory);
                    if observation_coherent {
                        break;
                    }
                }
                (before, agent_attempt, after) => {
                    terminals = match (before, after) {
                        (Err(error), _) | (_, Err(error)) => Err(error),
                        (Ok(_), Ok(after)) => Ok(after),
                    };
                    agents = agent_attempt;
                }
            }
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(25_u64 << attempt));
            }
        }
        let _ = sender.send(RestoreCompletion {
            port,
            dispatched_interaction,
            dispatched_registry_revision,
            dispatched_allowed_sessions: allowed_sessions,
            terminals,
            agents,
            observation_coherent,
        });
    });
}

pub(super) fn normalize_terminal_inventory(entries: &mut Vec<TerminalInventoryEntry>) {
    entries.sort_by_key(|entry| {
        (
            terminal_restore_sort_key(&entry.terminal),
            match entry.kind {
                TerminalKind::Agent => 0_u8,
                TerminalKind::Terminal => 1_u8,
            },
            entry.live,
        )
    });
    entries.dedup();
}

pub(super) fn restore_inventory_is_coherent(
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
    terminals: &[TerminalInventoryEntry],
    agents: &AgentInventory,
) -> bool {
    if agents.workspace_id != workspace {
        return false;
    }
    let in_scope = |terminal: &TerminalRef| {
        terminal.workspace_id == workspace
            && terminal
                .session_id
                .is_none_or(|session| allowed_sessions.contains(&session))
    };
    let live_agent_entries = terminals
        .iter()
        .filter(|entry| entry.live && entry.kind == TerminalKind::Agent)
        .filter(|entry| in_scope(&entry.terminal))
        .collect::<Vec<_>>();
    if terminals.iter().any(|entry| !in_scope(&entry.terminal)) {
        return false;
    }
    if agents
        .runtimes
        .iter()
        .any(|item| !in_scope(&item.runtime.terminal))
    {
        return false;
    }
    if terminals.iter().enumerate().any(|(index, entry)| {
        terminals[index + 1..]
            .iter()
            .any(|other| entry.terminal.fences(&other.terminal))
    }) {
        return false;
    }
    let live_runtimes = agents
        .runtimes
        .iter()
        .filter(|item| item.state == AgentRuntimeInventoryState::Live)
        .filter(|item| in_scope(&item.runtime.terminal))
        .collect::<Vec<_>>();
    if live_runtimes.iter().enumerate().any(|(index, item)| {
        live_runtimes[index + 1..]
            .iter()
            .any(|other| other.continuation == item.continuation)
    }) {
        return false;
    }
    live_agent_entries.iter().all(|entry| {
        live_runtimes
            .iter()
            .filter(|item| item.runtime.terminal.fences(&entry.terminal))
            .count()
            == 1
    }) && live_runtimes.iter().all(|item| {
        live_agent_entries
            .iter()
            .filter(|entry| entry.terminal.fences(&item.runtime.terminal))
            .count()
            == 1
    })
}

pub(super) fn pane_restore_targets(
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
    agents: AgentTabProjection,
    terminals: &[TerminalInventoryEntry],
    current_selected: Option<&TerminalRef>,
    interrupted: Vec<InterruptedTab>,
    saved_selections: &BTreeMap<Option<SessionId>, AgentContinuationRef>,
) -> Vec<PaneRestoreTarget> {
    let mut targets: BTreeMap<
        Option<SessionId>,
        (
            Vec<crate::usecase::application::pane::LivePane>,
            Option<TerminalRef>,
        ),
    > = BTreeMap::new();
    for target in agents.targets {
        let selected = target.selected.and_then(|selected| {
            target
                .tabs
                .iter()
                .find(|slot| slot.continuation == selected)
                .map(|slot| slot.terminal.clone())
        });
        let entry = targets.entry(target.session_id).or_default();
        entry.0.extend(target.tabs.into_iter().map(|slot| {
            crate::usecase::application::pane::LivePane {
                terminal: slot.terminal,
                kind: PaneKind::Agent,
            }
        }));
        entry.1 = selected;
    }
    targets.entry(None).or_default();
    for session in allowed_sessions {
        targets.entry(Some(*session)).or_default();
    }

    let mut generic = terminals
        .iter()
        .filter(|entry| entry.live && entry.kind == TerminalKind::Terminal)
        .filter(|entry| entry.terminal.workspace_id == workspace)
        // Root generic terminals are projected only by the dedicated bottom
        // drawer; managed-session terminals remain Closeup panes.
        .filter(|entry| {
            entry
                .terminal
                .session_id
                .is_none_or(|session| allowed_sessions.contains(&session))
        })
        .cloned()
        .collect::<Vec<_>>();
    generic.sort_by_key(|entry| terminal_restore_sort_key(&entry.terminal));
    for entry in generic {
        let target = targets.entry(entry.terminal.session_id).or_default();
        if !target
            .0
            .iter()
            .any(|pane| pane.terminal.fences(&entry.terminal))
        {
            target.0.push(crate::usecase::application::pane::LivePane {
                terminal: entry.terminal,
                kind: PaneKind::Terminal,
            });
        }
    }
    // Interrupted history joins its own scope's entry. A lineage whose session
    // is out of scope is already excluded by the projection.
    let mut histories: BTreeMap<Option<SessionId>, Vec<InterruptedTab>> = BTreeMap::new();
    for tab in interrupted {
        targets.entry(tab.session_id).or_default();
        histories.entry(tab.session_id).or_default().push(tab);
    }
    targets
        .into_iter()
        .map(|(session, (panes, selected))| {
            let interrupted = histories.remove(&session).unwrap_or_default();
            let selected_interrupted = if let Some(saved) = saved_selections.get(&session).copied()
            {
                let mut present = false;
                for tab in &interrupted {
                    if tab.continuation == saved {
                        present = true;
                        break;
                    }
                }
                present.then_some(saved)
            } else {
                None
            };
            let selected = selected
                .or_else(|| {
                    current_selected
                        .filter(|terminal| terminal.session_id == session)
                        .filter(|terminal| panes.iter().any(|pane| pane.terminal.fences(terminal)))
                        .cloned()
                })
                .or_else(|| {
                    panes
                        .iter()
                        .find(|pane| pane.kind == PaneKind::Terminal)
                        .or_else(|| panes.first())
                        .map(|pane| pane.terminal.clone())
                });
            PaneRestoreTarget {
                target: session.map_or(Target::Root(workspace), Target::Session),
                panes,
                selected,
                selected_interrupted,
                interrupted,
            }
        })
        .collect()
}

pub(super) fn terminal_restore_sort_key(
    terminal: &TerminalRef,
) -> (String, String, String, String, String) {
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

/// Project only generic additions when Agent intent persistence is unavailable.
/// The append-only runtime path preserves all existing panes and selection; a
/// later successful coherent observation owns authoritative membership/order.
pub(super) fn generic_restore_targets(
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
    terminals: &[TerminalInventoryEntry],
    runtime: &WorkspaceRuntime,
) -> Vec<PaneRestoreTarget> {
    let focused = runtime.focused_terminal();
    pane_restore_targets(
        workspace,
        allowed_sessions,
        AgentTabProjection::default(),
        terminals,
        focused.as_ref(),
        Vec::new(),
        &BTreeMap::new(),
    )
    .into_iter()
    .filter(|target| !target.panes.is_empty())
    .collect()
}

pub(super) fn apply_restore_completion(
    completion: RestoreCompletion,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    workspace: WorkspaceId,
    allowed_sessions: &BTreeSet<SessionId>,
) -> RestoreApply {
    let RestoreCompletion {
        port,
        dispatched_interaction,
        dispatched_registry_revision,
        dispatched_allowed_sessions,
        terminals,
        agents,
        observation_coherent,
    } = completion;
    // A partial or cross-RPC-inconsistent observation is an outage outcome even
    // when the user also moved the runtime fence. Transport failure must keep
    // controller backoff/notice semantics and cannot be converted into an
    // immediate fence retry by key activity.
    if !observation_coherent || terminals.is_err() || agents.is_err() {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::TransportFailed,
        };
    }
    if dispatched_allowed_sessions != *allowed_sessions {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::FenceRejected,
        };
    }
    if runtime.restore_fence() != (dispatched_interaction, dispatched_registry_revision) {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::FenceRejected,
        };
    }
    let terminals = terminals.expect("coherent restore checked terminal transport");
    let agents = agents.expect("coherent restore checked Agent transport");
    ui.agent_inventory = Some(agents.clone());
    ui.material_revision = ui.material_revision.saturating_add(1);
    // The interrupted projection reads the same coherent observation as the live
    // one, before the intent mutation consumes it.
    let interrupted = crate::usecase::application::interrupted_tab::project(
        &agents,
        workspace,
        allowed_sessions,
        &ui.agent_slot_order(),
        &ui.agent_dismissed(),
        &BTreeSet::new(),
    )
    .tabs;
    let observation = match ui.observe_agent_tabs(terminals.clone(), agents) {
        Ok(observation) => observation,
        Err(error) => {
            let restorable = ui.restorable_terminal_inventory(&terminals);
            let targets =
                generic_restore_targets(workspace, allowed_sessions, &restorable, runtime);
            let _ = runtime.append_restore_snapshot(
                dispatched_interaction,
                dispatched_registry_revision,
                targets,
            );
            return RestoreApply {
                port,
                outcome: RestoreJobOutcome::IntentFailed(error),
            };
        }
    };
    if !observation.cas_accepted {
        return RestoreApply {
            port,
            outcome: RestoreJobOutcome::FenceRejected,
        };
    }
    ui.reconcile_closed_generic_terminals(&terminals);
    let restorable = ui.restorable_terminal_inventory(&terminals);
    let selected = runtime.focused_terminal();
    let mut saved_selections = BTreeMap::new();
    if let Some(context) = ui.agent_tab_intent.as_ref() {
        for target in &context.state.targets {
            if let Some(selected) = target.selected {
                saved_selections.insert(target.session_id, selected);
            }
        }
    }
    let mut targets = pane_restore_targets(
        workspace,
        allowed_sessions,
        observation.projection,
        &restorable,
        selected.as_ref(),
        interrupted,
        &saved_selections,
    );
    runtime.preserve_workflow_selection(&mut targets);
    let fence_accepted = runtime.restore_snapshot(
        dispatched_interaction,
        dispatched_registry_revision,
        targets,
    );
    debug_assert!(
        fence_accepted,
        "restore fence cannot change during synchronous intent projection"
    );
    RestoreApply {
        port,
        outcome: RestoreJobOutcome::Applied,
    }
}

#[cfg(test)]
pub(super) fn restore_open_panes(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    geometry: Geometry,
) {
    let Ok(entries) = ui.list_open_terminals() else {
        return;
    };
    let mut grouped: BTreeMap<Option<SessionId>, Vec<crate::usecase::application::pane::LivePane>> =
        BTreeMap::new();
    for entry in entries.iter().filter(|entry| entry.live) {
        let panes = grouped.entry(entry.terminal.session_id).or_default();
        if !panes
            .iter()
            .any(|pane| pane.terminal.fences(&entry.terminal))
        {
            panes.push(crate::usecase::application::pane::LivePane {
                terminal: entry.terminal.clone(),
                kind: match entry.kind {
                    TerminalKind::Agent => PaneKind::Agent,
                    TerminalKind::Terminal => PaneKind::Terminal,
                },
            });
        }
    }
    let workspace = ui
        .agent
        .as_ref()
        .map_or(WorkspaceId::new(), |agent| agent.workspace);
    let targets = grouped
        .into_iter()
        .map(|(session, panes)| PaneRestoreTarget {
            target: session.map_or(Target::Root(workspace), Target::Session),
            selected: panes.first().map(|pane| pane.terminal.clone()),
            selected_interrupted: None,
            panes,
            interrupted: Vec::new(),
        })
        .collect();
    let (interaction, revision) = runtime.restore_fence();
    let _ = runtime.restore_snapshot(interaction, revision, targets);
    for target in entries.into_iter().filter(|entry| entry.live) {
        ui.start_terminal_session(target.terminal, geometry);
    }
}

pub(super) fn restore_workspace_session_focus(
    deck: &WorkspaceDeck,
    path: &Path,
    runtime: &mut WorkspaceRuntime,
) {
    if let Some(session) = deck.focused_session_for_path(path) {
        let _ = runtime.apply_event(AppEvent::FocusSession(session));
    }
}

/// Re-enter Closeup after a keyboard-driven project transition. This runs after
/// session/lifecycle synchronization so an unusable cached row never opens.
pub(super) fn restore_workspace_closeup(
    deck: &mut WorkspaceDeck,
    path: &Path,
    runtime: &mut WorkspaceRuntime,
) {
    let Some(session) = deck.take_closeup_session(path) else {
        return;
    };
    let _ = runtime.apply_event(AppEvent::FocusSession(session));
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Enter));
}

pub(super) fn restore_prepared_workspace(
    loader: &mut Option<&mut dyn WorkspaceLoader>,
    current: &Path,
) {
    let Some(loader) = loader.as_mut() else {
        return;
    };
    let _ = (**loader).activate_prepared(current);
}

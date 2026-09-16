//! Home frame loop が使う daemon transport の調整役（session worker・pane 起動・terminal stream・metrics）。route も selection も持たない。

use std::sync::mpsc;

use super::{
    AgentCommandPort, AgentContext, AgentContinuationRef, AgentInventory, AgentStreamPort,
    AgentTabIntent, AgentTabIntentContext, AgentTabIntentError, AgentTabIntentMutation,
    AgentTabIntentPort, AgentTabObservation, AgentTabProjection, BTreeMap, BTreeSet,
    DETACHED_TERMINAL_LIMIT, ExternalTerminalPort, Geometry, MAX_BACKGROUND_EXITS_PER_FRAME,
    PANE_LAUNCH_FIRST, PaneLaunch, PaneLaunchCommandPort, PaneLaunchCompletion, PendingCreate,
    ProviderResumeProjection, Receiver, RetainedRowMotion, Sender, SessionCommandCompletion,
    SessionCommandPort, SessionId, SessionState, TerminalBuffer, TerminalInputModes,
    TerminalInventoryEntry, TerminalKind, TerminalPoint, TerminalRef, TerminalSelection,
    TerminalSession, TerminalViewProjection, UnavailableExternalTerminalPort,
    UnavailablePaneLaunchPort, VecDeque, WorkspaceId, WorkspaceView,
};

/// daemon IO transport that the controller runtime keeps alongside its
/// [`WorkspaceRuntime`]: the session-create worker, the daemon-authoritative
/// session cache ([`WorkspaceView`]), pane launch workers, and live terminal
/// streams. Daemon metrics / git diffs are refluxed separately through
/// [`MetricsBackend`]. Home row state, input, and rendering belong to
/// the controller (`AppState`/`render_home`), not here.
pub(super) struct WorkspaceIoRuntime {
    pub(super) workspace: WorkspaceView,
    /// Shared daemon boundary. Admission allows one lifecycle worker at a time;
    /// snapshot revisions additionally fence stale authoritative observations.
    pub(super) session_commands: std::sync::Arc<dyn SessionCommandPort>,
    pub(super) last_session_revision: u64,
    /// Non-sensitive interrupted/resume state received from the daemon.
    pub(super) agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
    /// Latest coherent workspace-wide Agent inventory received by the restore
    /// lane. Kept as draw material for the read-only daemon status modal.
    pub(super) agent_inventory: Option<AgentInventory>,
    pub(super) material_revision: u64,
    pub(super) session_completions: Receiver<SessionCommandCompletion>,
    pub(super) session_completion_sender: Sender<SessionCommandCompletion>,
    /// Monotonic fence for the one admitted session command. A delayed or
    /// synthetic completion can never return its port into a newer command.
    pub(super) next_session_command: u64,
    pub(super) active_session_command: Option<u64>,
    /// Session displayed as a removal skeleton until its daemon command returns.
    pub(super) removing_session: Option<SessionId>,
    /// An in-flight create's controller token and the name drawn in its sidebar
    /// skeleton (`document/03-tui.md`). Its completion can reflux a failure to
    /// the reducer as an [`OperationResult`]. `Some` only while a create worker
    /// owns the admission slot, so the skeleton clears when its result lands.
    pub(super) creating_session: Option<PendingCreate>,
    pub(super) agent: Option<AgentContext>,
    pub(super) external_terminal: Box<dyn ExternalTerminalPort>,
    /// Shared launch client. Workers borrow it through the `Arc`, so the
    /// resident stream port stays with the live panes and a worker that hangs,
    /// panics, or loses its completion cannot take the capability away.
    pub(super) pane_launch_commands: std::sync::Arc<dyn PaneLaunchCommandPort>,
    /// Launches admitted and rendered as pending, oldest first. Bounded by
    /// [`PANE_LAUNCH_QUEUE_LIMIT`]; a request beyond the bound completes
    /// immediately as Busy instead of joining the queue.
    pub(super) pane_launches: Vec<PaneLaunch>,
    pub(super) pane_completions: Receiver<PaneLaunchCompletion>,
    pub(super) pane_completion_sender: Sender<PaneLaunchCompletion>,
    /// Monotonic fence for the one admitted launch worker. A late, duplicate, or
    /// unadmitted completion can never free a newer worker's slot.
    pub(super) next_pane_launch: u64,
    pub(super) active_pane_launch: Option<u64>,
    /// Live coordinators for terminals visible in the current frame. This is
    /// normally the selected foreground terminal; Director additionally keeps
    /// the dimmed managed-session preview attached. Hidden and unselected tabs
    /// retain only their stable pane identity.
    pub(super) terminals: Vec<TerminalSession>,
    /// Recently detached coordinators, oldest first. Keeping the coordinator
    /// preserves its connection-local input ledger and unresolved input fence.
    pub(super) detached_terminals: VecDeque<TerminalSession>,
    /// Generic terminals the user logically closed in this workspace UI. The
    /// daemon keeps their PTYs alive, so a later inventory must not immediately
    /// recreate their tabs. A fresh workspace UI starts empty and restores them
    /// again; an explicit `terminal open` also removes the exact fence.
    pub(super) closed_generic_terminals: BTreeSet<TerminalRef>,
    pub(super) terminal_reconnected: bool,
    pub(super) terminal_size: (usize, usize),
    pub(super) agent_tab_intent: Option<AgentTabIntentContext>,
    /// A successful durable Reopen requests one fresh coherent daemon
    /// observation. It never projects from an inventory cached before a later
    /// pane admission.
    pub(super) agent_observation_requested: bool,
    /// A successful Agent launch/resume or terminal exit changes daemon
    /// inventory. Unlike a display-only observation request, this must schedule
    /// one follow-up when an older restore snapshot is already in flight.
    pub(super) agent_inventory_change_observation_requested: bool,
}

impl WorkspaceIoRuntime {
    pub(super) fn new(
        workspace: WorkspaceView,
        session_commands: Box<dyn SessionCommandPort>,
    ) -> Self {
        let (session_completion_sender, session_completions) = mpsc::channel();
        let (pane_completion_sender, pane_completions) = mpsc::channel();
        Self {
            workspace,
            session_commands: std::sync::Arc::from(session_commands),
            last_session_revision: 0,
            agent_resumes: BTreeMap::new(),
            agent_inventory: None,
            material_revision: 0,
            session_completions,
            session_completion_sender,
            next_session_command: 1,
            active_session_command: None,
            removing_session: None,
            creating_session: None,
            agent: None,
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            pane_launch_commands: std::sync::Arc::new(UnavailablePaneLaunchPort),
            pane_launches: Vec::new(),
            pane_completions,
            pane_completion_sender,
            next_pane_launch: PANE_LAUNCH_FIRST,
            active_pane_launch: None,
            terminals: Vec::new(),
            detached_terminals: VecDeque::new(),
            closed_generic_terminals: BTreeSet::new(),
            terminal_reconnected: false,
            terminal_size: (0, 0),
            agent_tab_intent: None,
            agent_observation_requested: false,
            agent_inventory_change_observation_requested: false,
        }
    }

    pub(super) fn set_terminal_size(&mut self, height: usize, width: usize) {
        self.terminal_size = (height, width);
    }

    /// Bind the resident terminal stream port of one workspace. Pane launches
    /// use their own client ([`Self::with_pane_launch_port`]).
    pub(super) fn with_agent_context(
        mut self,
        workspace: WorkspaceId,
        sessions: Vec<SessionId>,
        port: Box<dyn AgentCommandPort>,
    ) -> Self {
        self.agent = Some(AgentContext {
            workspace,
            sessions,
            port,
        });
        self
    }

    /// Bind the dedicated client every pane launch worker borrows.
    pub(super) fn with_pane_launch_port(mut self, port: Box<dyn PaneLaunchCommandPort>) -> Self {
        self.pane_launch_commands = std::sync::Arc::from(port);
        self
    }

    pub(super) fn with_agent_tab_intent(
        mut self,
        workspace: WorkspaceId,
        allowed_sessions: BTreeSet<SessionId>,
        mut port: Box<dyn AgentTabIntentPort>,
    ) -> Self {
        let (state, load_error) = match port.load(workspace) {
            Ok(state) => (state, None),
            Err(error) => (AgentTabIntent::empty(workspace), Some(error)),
        };
        self.agent_tab_intent = Some(AgentTabIntentContext {
            workspace,
            allowed_sessions,
            state,
            port,
            visible_agents: Vec::new(),
            load_error,
        });
        self
    }

    pub(super) fn take_agent_tab_intent_load_error(&mut self) -> Option<AgentTabIntentError> {
        self.agent_tab_intent
            .as_mut()
            .and_then(|context| context.load_error.take())
    }

    pub(super) fn with_agent_resumes(
        mut self,
        agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
    ) -> Self {
        self.agent_resumes = agent_resumes;
        self
    }

    pub(super) fn with_external_terminal(mut self, port: Box<dyn ExternalTerminalPort>) -> Self {
        self.external_terminal = port;
        self
    }

    /// Attach to a freshly launched daemon terminal and start streaming it.
    ///
    /// A failed attach still records the session so its safe feedback renders;
    /// it never spawns a local process.
    pub(super) fn start_terminal_session(&mut self, terminal: TerminalRef, geometry: Geometry) {
        if self
            .terminals
            .iter()
            .any(|session| session.terminal().fences(&terminal))
        {
            return;
        }
        if let Some(agent) = self.agent.as_mut() {
            let retained = self
                .detached_terminals
                .iter()
                .position(|session| session.terminal().fences(&terminal))
                .and_then(|position| self.detached_terminals.remove(position));
            let mut stream = AgentStreamPort(agent.port.as_mut());
            // Synchronize a retained coordinator to the currently visible
            // viewport before attach. At an unchanged geometry this is a no-op;
            // at a changed outer size it sends exactly one resize and fences the
            // checkpoint against that new size.
            let mut session = match retained {
                Some(mut session) => {
                    session.resize(&mut stream, geometry);
                    session
                }
                None => TerminalSession::new(terminal, geometry),
            };
            session.connect(&mut stream);
            self.terminals.push(session);
        }
    }

    /// Keep exactly the active target's selected foreground terminal attached.
    /// Every hidden background target and unselected tab remains detached.
    #[cfg(test)]
    pub(super) fn sync_foreground_terminal(
        &mut self,
        focused: Option<&TerminalRef>,
        geometry: Geometry,
    ) {
        let visible = focused
            .map(|terminal| vec![(terminal.clone(), geometry)])
            .unwrap_or_default();
        self.sync_visible_terminals(&visible);
    }

    /// Keep the bounded set of terminals participating in this Home composition
    /// attached, each at the geometry of the surface that owns it.
    ///
    /// Ordinary Home supplies one entry. An open workspace drawer supplies its
    /// root surface plus the managed-session terminal underneath it, even when
    /// the overlay covers every background cell. Input ownership is independent:
    /// only the runtime's focused terminal receives bytes.
    pub(super) fn sync_visible_terminals(&mut self, visible: &[(TerminalRef, Geometry)]) {
        let stale = self
            .terminals
            .iter()
            .filter(|session| {
                !visible
                    .iter()
                    .any(|(terminal, _)| session.terminal().fences(terminal))
            })
            .map(|session| session.terminal().clone())
            .collect::<Vec<_>>();
        for terminal in stale {
            self.close_terminal(&terminal);
        }

        for (terminal, geometry) in visible {
            if let Some(index) = self
                .terminals
                .iter()
                .position(|session| session.terminal().fences(terminal))
            {
                if let Some(agent) = self.agent.as_mut() {
                    self.terminals[index]
                        .resize(&mut AgentStreamPort(agent.port.as_mut()), *geometry);
                }
            } else {
                self.start_terminal_session(terminal.clone(), *geometry);
            }
        }
    }

    /// Ask the daemon for the runtimes still live in this workspace's scopes.
    /// A missing port (embedder) yields an empty inventory rather than an error,
    /// so restore simply finds nothing. A daemon failure is surfaced so the
    /// caller restores nothing instead of guessing.
    #[cfg(test)]
    pub(super) fn list_open_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, ()> {
        match self.agent.as_mut() {
            Some(agent) => agent.port.list_terminals().map_err(|_| ()),
            None => Ok(Vec::new()),
        }
    }

    #[cfg(test)]
    pub(super) fn resize_terminals(&mut self, geometry: Geometry) {
        let Some(agent) = self.agent.as_mut() else {
            return;
        };
        for session in &mut self.terminals {
            session.resize(&mut AgentStreamPort(agent.port.as_mut()), geometry);
        }
    }

    /// Forward raw passthrough bytes to the live terminal `terminal`. Returns an
    /// error only when this workspace has no daemon stream at all or the matching
    /// session cannot accept the bytes — a pane launch in flight never makes the
    /// stream unavailable, so a focused keystroke is not lost to a busy port.
    pub(super) fn send_terminal_bytes(
        &mut self,
        terminal: &TerminalRef,
        bytes: &[u8],
    ) -> Result<(), String> {
        let Some(agent) = self.agent.as_mut() else {
            return Err("terminal stream is unavailable".to_owned());
        };
        let Some(session) = self
            .terminals
            .iter_mut()
            .find(|session| session.terminal().fences(terminal))
        else {
            return Err("terminal session is no longer available".to_owned());
        };
        match session.send_input(&mut AgentStreamPort(agent.port.as_mut()), bytes) {
            Ok(()) => Ok(()),
            Err(error) => Err(error.message()),
        }
    }

    pub(super) fn clear_terminal_for_user(&mut self, terminal: &TerminalRef) -> bool {
        self.terminals
            .iter_mut()
            .find(|session| session.terminal().fences(terminal))
            .is_some_and(TerminalSession::clear_for_user)
    }

    /// Poll every attached terminal once and return the refs of those the daemon
    /// reports as exited. Polling all of them (not just the focused pane) is what
    /// lets a background tab whose shell ran `exit` be detected and closed.
    pub(super) fn poll_all_terminals(&mut self) -> Vec<TerminalRef> {
        let Some(agent) = self.agent.as_mut() else {
            return Vec::new();
        };
        let port = agent.port.as_mut();
        let mut reconnected = false;
        let exited = self
            .terminals
            .iter_mut()
            .filter_map(|session| {
                let before = session.state();
                session.poll(&mut AgentStreamPort(port));
                // Any pane that streams again is a reconnection, not only one
                // that was waiting on an unavailable daemon: a refused attach
                // and a refused stream recover through the same re-attach, and
                // the user is owed the same feedback for all of them.
                if before != SessionState::Live && session.state() == SessionState::Live {
                    reconnected = true;
                }
                (session.state() == SessionState::Exited).then(|| session.terminal().clone())
            })
            .collect();
        self.terminal_reconnected |= reconnected;
        exited
    }

    pub(super) fn take_terminal_row_motions(
        &mut self,
    ) -> Vec<(TerminalRef, Vec<RetainedRowMotion>)> {
        self.terminals
            .iter_mut()
            .filter_map(|session| {
                let motions = session.take_retained_row_motions();
                (!motions.is_empty()).then(|| (session.terminal().clone(), motions))
            })
            .collect()
    }

    /// Hand the detached background tabs to the port's bounded scope-inventory
    /// lane and drain the exits it has observed since the last frame.
    ///
    /// This is the whole detached-background contract: metadata only, per scope,
    /// off the render thread. No `Attach` and no terminal-specific `Resume` is
    /// ever sent for a detached tab, and the returned refs are exactly the tabs
    /// whose runtime the daemon no longer reports as live.
    pub(super) fn sync_background_terminals(
        &mut self,
        background: &[TerminalRef],
    ) -> Vec<TerminalRef> {
        let Some(agent) = self.agent.as_mut() else {
            return Vec::new();
        };
        // Director's dimmed managed pane is background with respect to input,
        // but visible and attached with respect to output. Its stream reports
        // exit directly, so only genuinely detached tabs belong in the scope
        // inventory lane.
        let detached = background
            .iter()
            .filter(|terminal| {
                !self
                    .terminals
                    .iter()
                    .any(|session| session.terminal().fences(terminal))
            })
            .cloned()
            .collect::<Vec<_>>();
        agent.port.watch_background_terminals(&detached);
        agent
            .port
            .take_exited_background_terminals(MAX_BACKGROUND_EXITS_PER_FRAME)
    }

    pub(super) fn take_terminal_reconnected(&mut self) -> bool {
        std::mem::take(&mut self.terminal_reconnected)
    }

    /// Release a terminal's client subscription and retain its coordinator in a
    /// bounded LRU. The daemon keeps the process and connection-local input
    /// ledger; a later attach therefore preserves ordering and unresolved input.
    pub(super) fn close_terminal(&mut self, terminal: &TerminalRef) {
        let Some(position) = self
            .terminals
            .iter()
            .position(|session| session.terminal().fences(terminal))
        else {
            return;
        };
        let mut session = self.terminals.remove(position);
        if let Some(agent) = self.agent.as_mut() {
            session.detach(&mut AgentStreamPort(agent.port.as_mut()));
        }
        self.detached_terminals
            .retain(|retained| !retained.terminal().fences(terminal));
        self.detached_terminals.push_back(session);
        while self.detached_terminals.len() > DETACHED_TERMINAL_LIMIT {
            self.detached_terminals.pop_front();
        }
    }

    /// Hide one generic terminal while its requested shell exit is still being
    /// observed, and detach this client's subscription.
    pub(super) fn close_generic_terminal(&mut self, terminal: &TerminalRef) {
        self.closed_generic_terminals.insert(terminal.clone());
        self.close_terminal(terminal);
    }

    /// Whether a launch completion points back to a shell whose exit has not
    /// reached coherent inventory yet. Reusing it would resurrect the exact
    /// scrollback the user just closed.
    pub(super) fn generic_terminal_is_closing(&self, terminal: &TerminalRef) -> bool {
        self.closed_generic_terminals
            .iter()
            .any(|closed| closed.fences(terminal))
    }

    /// A coherent inventory proves which process-local close fences can still
    /// matter. Exited terminals no longer need suppression, keeping this set
    /// bounded by the daemon's live generic inventory.
    pub(super) fn reconcile_closed_generic_terminals(
        &mut self,
        terminals: &[TerminalInventoryEntry],
    ) {
        self.closed_generic_terminals.retain(|closed| {
            terminals.iter().any(|entry| {
                entry.live && entry.kind == TerminalKind::Terminal && entry.terminal.fences(closed)
            })
        });
    }

    /// Return the authoritative inventory with only this UI's logically closed
    /// generic terminals hidden. Agent rows and every other terminal row stay
    /// unchanged for reconciliation.
    pub(super) fn restorable_terminal_inventory(
        &self,
        terminals: &[TerminalInventoryEntry],
    ) -> Vec<TerminalInventoryEntry> {
        terminals
            .iter()
            .filter(|entry| {
                entry.kind != TerminalKind::Terminal
                    || !self.closed_generic_terminals.contains(&entry.terminal)
            })
            .cloned()
            .collect()
    }

    pub(super) fn agent_continuation_for(
        &self,
        terminal: &TerminalRef,
    ) -> Option<AgentContinuationRef> {
        self.agent_tab_intent.as_ref().and_then(|context| {
            context
                .state
                .targets
                .iter()
                .find_map(|target| {
                    target
                        .tabs
                        .iter()
                        .find(|slot| slot.terminal.fences(terminal))
                        .map(|slot| slot.continuation)
                })
                .or_else(|| {
                    context
                        .visible_agents
                        .iter()
                        .find(|(visible, _)| visible.fences(terminal))
                        .map(|(_, continuation)| *continuation)
                })
        })
    }

    pub(super) fn observe_agent_tabs(
        &mut self,
        terminals: Vec<TerminalInventoryEntry>,
        agents: AgentInventory,
    ) -> Result<AgentTabObservation, AgentTabIntentError> {
        let Some(context) = self.agent_tab_intent.as_mut() else {
            return Ok(AgentTabObservation {
                projection: AgentTabProjection::default(),
                cas_accepted: true,
            });
        };
        let commit = context.port.mutate(
            context.workspace,
            context.state.revision,
            AgentTabIntentMutation::Observe {
                terminals,
                agents,
                allowed_sessions: context.allowed_sessions.clone(),
            },
        )?;
        context.state = commit.intent;
        let projection = commit.projection.unwrap_or_default();
        if commit.mutation_applied {
            context.visible_agents = projection
                .targets
                .iter()
                .flat_map(|target| &target.tabs)
                .map(|slot| (slot.terminal.clone(), slot.continuation))
                .collect();
        }
        Ok(AgentTabObservation {
            projection,
            cas_accepted: commit.mutation_applied,
        })
    }

    pub(super) fn mutate_agent_intent(
        &mut self,
        mutation: AgentTabIntentMutation,
    ) -> Result<(), AgentTabIntentError> {
        let Some(context) = self.agent_tab_intent.as_mut() else {
            return Ok(());
        };
        let commit = context
            .port
            .mutate(context.workspace, context.state.revision, mutation)?;
        context.state = commit.intent;
        if !commit.mutation_applied {
            return Err(AgentTabIntentError::ConcurrentChange);
        }
        Ok(())
    }

    pub(super) fn request_agent_observation(&mut self) {
        self.agent_observation_requested = true;
    }

    pub(super) fn take_agent_observation_request(&mut self) -> bool {
        std::mem::take(&mut self.agent_observation_requested)
    }

    pub(super) fn request_agent_inventory_change_observation(&mut self) {
        self.agent_inventory_change_observation_requested = true;
    }

    pub(super) fn take_agent_inventory_change_observation_request(&mut self) -> bool {
        std::mem::take(&mut self.agent_inventory_change_observation_requested)
    }

    pub(super) fn agent_inventory(&self) -> Option<&AgentInventory> {
        self.agent_inventory.as_ref()
    }

    /// Opening the daemon modal starts from an explicit loading projection and
    /// asks the existing coalesced restore lane for one fresh coherent snapshot.
    pub(super) fn refresh_agent_inventory(&mut self) {
        self.agent_inventory = None;
        self.material_revision = self.material_revision.saturating_add(1);
        self.request_agent_observation();
    }

    /// The saved Agent slot order of the whole workspace, flattened across
    /// targets. It gives a restored interrupted tab the position the user last
    /// saw it in (#506 slots keyed by lineage).
    pub(super) fn agent_slot_order(&self) -> Vec<AgentContinuationRef> {
        self.agent_tab_intent
            .as_ref()
            .map_or_else(Vec::new, |context| {
                context
                    .state
                    .targets
                    .iter()
                    .flat_map(|target| &target.tabs)
                    .map(|slot| slot.continuation)
                    .collect()
            })
    }

    /// Lineages explicitly removed from the tab strip. Daemon inventory still
    /// owns runtime liveness, while this local intent owns visibility.
    pub(super) fn agent_dismissed(&self) -> BTreeSet<AgentContinuationRef> {
        self.agent_tab_intent
            .as_ref()
            .map_or_else(BTreeSet::new, |context| context.state.dismissed.clone())
    }

    pub(super) fn has_agent_intent_for(&self, session_id: Option<SessionId>) -> bool {
        self.agent_tab_intent.as_ref().is_some_and(|context| {
            context
                .state
                .targets
                .iter()
                .find(|target| target.session_id == session_id)
                .is_some_and(|target| !target.tabs.is_empty())
        })
    }

    pub(super) fn set_allowed_agent_sessions(
        &mut self,
        sessions: impl IntoIterator<Item = SessionId>,
    ) {
        let sessions = sessions.into_iter().collect::<BTreeSet<_>>();
        let changed = self
            .agent_tab_intent
            .as_ref()
            .is_some_and(|context| context.allowed_sessions != sessions);
        if let Some(context) = self.agent_tab_intent.as_mut() {
            context.allowed_sessions = sessions;
        }
        if changed {
            // Lifecycle membership is authoritative for target retention. Use
            // the same coalesced controller request as Reopen so an idle,
            // already-successful controller observes removals exactly once;
            // an in-flight job is fenced and an outage keeps its backoff.
            self.request_agent_observation();
        }
    }

    /// Project the already-polled rows for `terminal`, optionally highlighting an
    /// in-progress selection. Returns `None` when no attached session matches.
    #[cfg(test)]
    pub(super) fn terminal_rows(
        &self,
        terminal: &TerminalRef,
        selection: Option<&TerminalSelection>,
    ) -> Option<Vec<String>> {
        let session = self
            .terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))?;
        Some(match selection {
            Some(selection) => session.display_rows_with_scrollback_selection(selection),
            None => session.display_rows_with_scrollback(),
        })
    }

    pub(super) fn terminal_row_extent(
        &self,
        terminal: &TerminalRef,
        selection: Option<&TerminalSelection>,
    ) -> Option<(TerminalBuffer, u64, usize)> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(|session| {
                let rows = match selection {
                    Some(selection) => session.display_row_count_selection(selection),
                    None => session.display_row_count(),
                };
                (session.display_buffer(), session.display_row_origin(), rows)
            })
    }

    pub(super) fn terminal_projection_key(&self, terminal: &TerminalRef) -> Option<u64> {
        self.terminals
            .iter()
            .chain(self.detached_terminals.iter())
            .find(|session| session.terminal().fences(terminal))
            .map(TerminalSession::projection_key)
    }

    pub(super) fn terminal_input_modes(
        &self,
        terminal: &TerminalRef,
    ) -> Option<TerminalInputModes> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(TerminalSession::input_modes)
    }

    /// Snapshot the retained rows of a background terminal without changing its
    /// attachment or scroll controls. A workspace drawer keeps the managed pane
    /// underneath it attached, so this projection advances as its live stream is
    /// drained even while the overlay covers it.
    pub(super) fn retained_terminal_view(
        &self,
        terminal: &TerminalRef,
        viewport_rows: usize,
    ) -> Option<TerminalViewProjection> {
        let session = self
            .terminals
            .iter()
            .chain(self.detached_terminals.iter())
            .find(|session| session.terminal().fences(terminal))?;
        let total_rows = session.display_row_count();
        let start = total_rows.saturating_sub(viewport_rows);
        Some(TerminalViewProjection {
            rows: session.display_row_window(start, total_rows),
            row_offset: start,
            total_rows,
            scroll: 0,
            feedback: session.error().map(str::to_owned),
        })
    }

    pub(super) fn terminal_row_window(
        &self,
        terminal: &TerminalRef,
        start: usize,
        end: usize,
        selection: Option<&TerminalSelection>,
    ) -> Option<Vec<String>> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(|session| match selection {
                Some(selection) => session.display_row_window_selection(start, end, selection),
                None => session.display_row_window(start, end),
            })
    }

    /// The stable visible cells for `terminal`, snapshotted so a drag selection
    /// stays fixed while later output arrives. `None` when no session matches.
    pub(super) fn terminal_cells(&self, terminal: &TerminalRef) -> Option<Vec<String>> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(TerminalSession::cells)
    }

    pub(super) fn begin_terminal_selection(
        &self,
        terminal: &TerminalRef,
        anchor: TerminalPoint,
    ) -> Option<TerminalSelection> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(|session| session.begin_selection(anchor))
    }

    pub(super) fn terminal_error(&self, terminal: &TerminalRef) -> Option<&str> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .and_then(TerminalSession::error)
    }
}

//! Controller-driven Home runtime for the real terminal.
//!
//! `WorkspaceRuntime` owns the controller [`AppState`] and the target-scoped
//! [`PaneRegistry`], and is the single source of Home row state, live-pane
//! availability, and the `render_home` frame. It reuses the pure reducers
//! (`controller::update`, `pane::reduce_registry`, `pane::route_tab_command`)
//! so the real-terminal frame loop can delegate state, input, and rendering to
//! it instead of the legacy `Workspace` view.
//!
//! The shell (frame loop) keeps ownership of daemon IO: it launches panes,
//! polls terminals, and executes the returned [`Effect`]s, feeding the results
//! back through the pane-lifecycle methods here. This keeps the runtime pure and
//! unit-testable while the live-terminal machinery stays in the composition
//! shell.

use std::collections::BTreeMap;
use std::path::PathBuf;

use usagi_core::domain::id::{OperationId, SessionId, TerminalRef};
use usagi_core::usecase::client::DaemonMetrics;

use crate::presentation::views::workspace::{
    GitDiff, HomeProjection, ProjectedSession, TerminalViewProjection, render_home,
};
use crate::usecase::application::Key;
use crate::usecase::application::controller::{
    AppEvent, AppState, Effect, HomeMode, Route, TabDirection, Target, update,
};
use crate::usecase::application::pane::{
    PaneEvent, PaneInputOwner, PaneKind, PaneRegistry, PaneRegistryEffect, PaneRegistryEvent,
    PaneSelection, PaneState, PaneTab, PaneTabCommand, TabSelection, reduce_registry,
    route_tab_command,
};

use super::app_event_from_key;

/// The daemon transport work the shell owes a closed pane tab. A live tab must
/// have its client subscription detached; a still-pending launch must be dropped
/// before it spawns a detached daemon terminal behind the vanished placeholder.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CloseOutcome {
    /// The live terminal whose client subscription the shell must release.
    pub detach: Option<TerminalRef>,
    /// The pending launch the shell must cancel before it reaches the daemon.
    pub cancel: Option<OperationId>,
}

/// Home runtime backed by the controller reducer and pane registry.
pub struct WorkspaceRuntime {
    state: AppState,
    panes: PaneRegistry,
}

impl WorkspaceRuntime {
    /// Start a Home runtime for `workspace` with the daemon-authoritative
    /// `sessions`. The initial selection and active target are the workspace
    /// root, matching [`AppState::home`].
    #[must_use]
    pub fn new(workspace: usagi_core::domain::id::WorkspaceId, sessions: Vec<SessionId>) -> Self {
        let state = AppState::home(workspace, sessions);
        let panes = PaneRegistry::new(state.active());
        Self { state, panes }
    }

    /// The controller state driving Home rows, overlays, and markers.
    #[must_use]
    pub const fn state(&self) -> &AppState {
        &self.state
    }

    /// The active target's pane state, for `HomeProjection::with_pane`.
    #[must_use]
    pub fn active_pane(&self) -> &PaneState {
        self.panes.active_pane()
    }

    /// The pane registry, for callers that need per-target tab state.
    #[must_use]
    pub const fn panes(&self) -> &PaneRegistry {
        &self.panes
    }

    /// Translate a terminal [`Key`] into Home input and return the effects the
    /// shell must dispatch. Passthrough/pointer keys yield no effects; the shell
    /// gates live passthrough via [`WorkspaceRuntime::wants_live_input`] before
    /// calling this.
    #[must_use]
    pub fn handle_key(&mut self, key: Key) -> Vec<Effect> {
        match app_event_from_key(key) {
            Some(event) => self.apply_event(event),
            None => Vec::new(),
        }
    }

    /// Reduce one [`AppEvent`] (key, resize, tick, backend, completion) and
    /// return its effects, keeping the pane registry's active target and the
    /// live-pane flag in sync with the resulting controller state.
    #[must_use]
    pub fn apply_event(&mut self, event: AppEvent) -> Vec<Effect> {
        let effects = update(&mut self.state, event);
        self.follow_active_target();
        effects
    }

    /// Whether a live terminal currently owns keyboard input, so the shell
    /// forwards raw passthrough bytes to the PTY instead of the reducer. True
    /// only in Closeup with an available live pane whose tab (not the action
    /// modal) owns input.
    #[must_use]
    pub fn wants_live_input(&self) -> bool {
        matches!(self.state.route(), Route::Home(HomeMode::Closeup))
            && self.state.has_live_pane()
            && matches!(self.panes.input_owner(), PaneInputOwner::Tab)
    }

    /// The terminal the active pane's selected tab attaches to, if the selection
    /// is a live tab. The shell polls this terminal for the viewport and forwards
    /// passthrough bytes to it.
    #[must_use]
    pub fn focused_terminal(&self) -> Option<TerminalRef> {
        match self.panes.active_pane().selected() {
            PaneSelection::Tab(TabSelection::Live(terminal)) => Some(terminal.clone()),
            PaneSelection::Tab(TabSelection::Pending(_) | TabSelection::Ready(_))
            | PaneSelection::Target(_) => None,
        }
    }

    /// Record a pane open request as a pending placeholder for `target`.
    pub fn request_pane(
        &mut self,
        target: Target,
        operation: OperationId,
        kind: PaneKind,
    ) -> Vec<PaneRegistryEffect> {
        let effects = reduce_registry(
            &mut self.panes,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Request {
                    operation,
                    target,
                    kind,
                },
            },
        );
        self.sync_live_pane();
        effects
    }

    /// Promote a pending placeholder to a live tab once the daemon confirms the
    /// terminal identity.
    pub fn complete_pane(
        &mut self,
        target: Target,
        operation: OperationId,
        terminal: TerminalRef,
    ) -> Vec<PaneRegistryEffect> {
        let effects = reduce_registry(
            &mut self.panes,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Succeeded {
                    operation,
                    terminal,
                },
            },
        );
        self.sync_live_pane();
        effects
    }

    /// Drop a pending placeholder and surface a display-safe failure.
    pub fn fail_pane(
        &mut self,
        target: Target,
        operation: OperationId,
        message: String,
    ) -> Vec<PaneRegistryEffect> {
        let effects = reduce_registry(
            &mut self.panes,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Failed { operation, message },
            },
        );
        self.sync_live_pane();
        effects
    }

    /// Focus the live tab attached to `terminal` for `target`. The shell calls
    /// this after it opens a pane the user initiated, so the completed tab becomes
    /// the input owner and its viewport renders (completion alone never steals
    /// focus).
    pub fn focus_terminal(
        &mut self,
        target: Target,
        terminal: TerminalRef,
    ) -> Vec<PaneRegistryEffect> {
        let effects = reduce_registry(
            &mut self.panes,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(terminal))),
            },
        );
        self.sync_live_pane();
        effects
    }

    /// Remove a live tab the daemon reports as exited.
    pub fn exit_pane(&mut self, target: Target, terminal: TerminalRef) -> Vec<PaneRegistryEffect> {
        let effects = reduce_registry(
            &mut self.panes,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Exited(terminal),
            },
        );
        self.sync_live_pane();
        effects
    }

    /// Close the focused pane tab (Ctrl-O x). Returns the daemon transport work
    /// the shell must perform for the removed tab: `detach` a live terminal's
    /// client subscription, or `cancel` a still-pending launch before it spawns a
    /// detached daemon terminal. A target selection (no tab) is a no-op. The
    /// registry state and the live-pane flag stay in sync either way.
    pub fn close_focused_pane(&mut self) -> CloseOutcome {
        let outcome = match self.panes.active_pane().selected() {
            PaneSelection::Tab(TabSelection::Live(terminal)) => CloseOutcome {
                detach: Some(terminal.clone()),
                cancel: None,
            },
            PaneSelection::Tab(
                TabSelection::Pending(operation) | TabSelection::Ready(operation),
            ) => CloseOutcome {
                detach: None,
                cancel: Some(*operation),
            },
            PaneSelection::Target(_) => CloseOutcome::default(),
        };
        let _ = route_tab_command(&mut self.panes, PaneTabCommand::Close);
        self.sync_live_pane();
        outcome
    }

    /// Cycle the active pane's selected tab for an `Effect::SelectTab`. Only the
    /// tab owner (not the action modal) reacts, matching the reducer contract.
    pub fn select_tab(&mut self, direction: TabDirection) -> Vec<PaneRegistryEffect> {
        let Some(selection) = self.adjacent_tab(direction) else {
            return Vec::new();
        };
        let effects = route_tab_command(&mut self.panes, PaneTabCommand::Select(selection));
        self.sync_live_pane();
        effects
    }

    /// Mirror a controller [`Effect`]'s pane-visible intent into the registry
    /// before the shell executes the effect against daemon IO. `SelectTab`
    /// cycles the active tab; `OpenTerminal`/`LaunchAgent` record a pending
    /// placeholder keyed by the effect's operation, so the daemon completion the
    /// shell later routes to [`WorkspaceRuntime::complete_pane`] promotes the
    /// matching tab. Effects with no pane surface are ignored here.
    pub fn on_effect(&mut self, effect: &Effect) {
        match effect {
            Effect::SelectTab { direction } => {
                let _ = self.select_tab(*direction);
            }
            // A terminal only opens against a session pane. The workspace root
            // has no pane strip, so an `OpenTerminal` for `Target::Root` falls
            // through to the ignored arm below instead of stranding a placeholder
            // tab that no completion can ever promote.
            Effect::OpenTerminal {
                target: target @ Target::Session(_),
                operation_id,
                ..
            } => {
                let _ = self.request_pane(*target, *operation_id, PaneKind::Terminal);
            }
            Effect::LaunchAgent {
                session,
                operation_id,
                ..
            } => {
                let _ =
                    self.request_pane(Target::Session(*session), *operation_id, PaneKind::Agent);
            }
            _ => {}
        }
    }

    fn adjacent_tab(&self, direction: TabDirection) -> Option<TabSelection> {
        let pane = self.panes.active_pane();
        let tabs = pane.tabs();
        if tabs.is_empty() {
            return None;
        }
        let current = match pane.selected() {
            PaneSelection::Tab(selection) => {
                tabs.iter().position(|tab| tab_selection(tab) == *selection)
            }
            PaneSelection::Target(_) => None,
        };
        let index = match (current, direction) {
            (Some(index), TabDirection::Next) => (index + 1) % tabs.len(),
            (Some(index), TabDirection::Previous) => (index + tabs.len() - 1) % tabs.len(),
            (None, TabDirection::Next) => 0,
            (None, TabDirection::Previous) => tabs.len() - 1,
        };
        Some(tab_selection(&tabs[index]))
    }

    fn follow_active_target(&mut self) {
        if self.panes.active() != self.state.active() {
            let _ = reduce_registry(
                &mut self.panes,
                PaneRegistryEvent::SelectTarget(self.state.active()),
            );
        }
        self.sync_live_pane();
    }

    fn sync_live_pane(&mut self) {
        let live = matches!(self.state.active(), Target::Session(_))
            && self
                .panes
                .active_pane()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Live(_)));
        let _ = update(&mut self.state, AppEvent::LivePaneAvailability(live));
    }

    /// Build the Home frame from the controller state, pane strip, and the
    /// per-frame projection material the shell polls (metrics, git diffs, live
    /// terminal viewport). This is the only render path for the controller
    /// runtime.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        height: usize,
        width: usize,
        workspace_name: &str,
        root_cwd: impl Into<PathBuf>,
        sessions: &[ProjectedSession],
        metrics: Option<DaemonMetrics>,
        git_diffs: &BTreeMap<SessionId, GitDiff>,
        terminal_view: Option<TerminalViewProjection>,
    ) -> Vec<String> {
        let projection =
            HomeProjection::from_state(&self.state, workspace_name, root_cwd, sessions)
                .with_pane(self.panes.active_pane())
                .with_metrics(metrics)
                .with_git_diffs(git_diffs)
                .with_terminal_view(terminal_view);
        render_home(height, width, &projection)
    }
}

fn tab_selection(tab: &PaneTab) -> TabSelection {
    match tab {
        PaneTab::Pending(pending) => TabSelection::Pending(pending.operation),
        PaneTab::Live(live) => TabSelection::Live(live.terminal.clone()),
        PaneTab::Ready(pending) => TabSelection::Ready(pending.operation),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CloseOutcome, PaneEvent, PaneKind, PaneTab, TabSelection, WorkspaceRuntime, tab_selection,
    };
    use crate::usecase::application::Key;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, Effect, HomeMode, Route, TabDirection, Target,
    };
    use crate::usecase::application::pane::{
        LivePane, PaneRegistry, PaneRegistryEvent, PaneSelection, PendingPane, reduce_registry,
    };
    use crate::usecase::terminal_input::LiveTerminalAction;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use usagi_core::domain::id::{
        DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    };

    fn terminal_ref(workspace: WorkspaceId, session: SessionId) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        }
    }

    /// Drive the runtime into Closeup with the given session active.
    fn closeup_on(workspace: WorkspaceId, session: SessionId) -> WorkspaceRuntime {
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Down); // select the session
        let _ = runtime.handle_key(Key::Enter); // activate → Closeup
        assert_eq!(runtime.state().active(), Target::Session(session));
        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Closeup)
        ));
        runtime
    }

    #[test]
    fn new_starts_at_root_with_an_empty_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let runtime = WorkspaceRuntime::new(workspace, vec![session]);
        assert_eq!(runtime.state().active(), Target::Root(workspace));
        assert_eq!(runtime.panes().active(), Target::Root(workspace));
        assert!(runtime.active_pane().tabs().is_empty());
        assert!(!runtime.state().has_live_pane());
        assert!(!runtime.wants_live_input());
    }

    #[test]
    fn handle_key_moves_selection_and_ignores_passthrough() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        assert!(runtime.handle_key(Key::Down).is_empty());
        // Down selected the session; Enter now activates it.
        let effects = runtime.handle_key(Key::Enter);
        assert!(effects.is_empty());
        assert_eq!(runtime.state().active(), Target::Session(session));
        // Passthrough never reaches the reducer.
        assert!(runtime.handle_key(Key::Passthrough(vec![0x1b])).is_empty());
    }

    #[test]
    fn new_session_row_enter_emits_create_effect() {
        let workspace = WorkspaceId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        // Root, then + new session, activate it, then submit the form.
        let _ = runtime.handle_key(Key::Down); // + new session
        let _ = runtime.handle_key(Key::Enter); // open create form
        let _ = runtime.handle_key(Key::Char('a'));
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CreateSession { .. })),
            "{effects:?}"
        );
    }

    #[test]
    fn follow_active_target_switches_the_registry_entry() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let runtime = closeup_on(workspace, session);
        assert_eq!(runtime.panes().active(), Target::Session(session));
    }

    #[test]
    fn pane_lifecycle_tracks_live_availability() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let operation = OperationId::new();

        // A pending placeholder is not yet a live pane.
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        assert!(matches!(
            runtime.active_pane().tabs().first(),
            Some(PaneTab::Pending(_))
        ));
        assert!(!runtime.state().has_live_pane());

        // Completing it promotes the tab and arms live input.
        let terminal = terminal_ref(workspace, session);
        let _ = runtime.complete_pane(target, operation, terminal.clone());
        assert!(matches!(
            runtime.active_pane().tabs().first(),
            Some(PaneTab::Live(_))
        ));
        assert!(runtime.state().has_live_pane());
        assert!(runtime.wants_live_input());

        // The daemon reporting the terminal exit clears the live pane.
        let _ = runtime.exit_pane(target, terminal);
        assert!(runtime.active_pane().tabs().is_empty());
        assert!(!runtime.state().has_live_pane());
    }

    #[test]
    fn focused_terminal_follows_the_selected_live_tab() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        assert_eq!(runtime.focused_terminal(), None); // no tabs yet

        let operation = OperationId::new();
        let terminal = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        assert_eq!(runtime.focused_terminal(), None); // pending, not live

        let _ = runtime.complete_pane(target, operation, terminal.clone());
        assert_eq!(runtime.focused_terminal(), None); // promoted, not yet focused

        // Completion promotes the tab but does not steal focus; focusing it does.
        let _ = runtime.focus_terminal(target, terminal.clone());
        assert_eq!(runtime.focused_terminal(), Some(terminal));
    }

    #[test]
    fn failed_pane_drops_the_placeholder_with_a_safe_message() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let operation = OperationId::new();
        let _ = runtime.request_pane(target, operation, PaneKind::Agent);
        let _ = runtime.fail_pane(target, operation, "safe failure".to_owned());
        assert!(runtime.active_pane().tabs().is_empty());
        assert_eq!(runtime.active_pane().error(), Some("safe failure"));
    }

    #[test]
    fn select_tab_cycles_between_live_tabs() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);

        let first_op = OperationId::new();
        let first = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, first_op, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, first_op, first.clone());
        let second_op = OperationId::new();
        let second = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, second_op, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, second_op, second.clone());

        // Anchor the selection on the first tab, then Next advances to the second.
        let _ = reduce_registry(
            runtime_panes_mut(&mut runtime),
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::Select(PaneSelection::Tab(TabSelection::Live(first.clone()))),
            },
        );
        let _ = runtime.select_tab(TabDirection::Next);
        assert_eq!(
            runtime.active_pane().selected(),
            &PaneSelection::Tab(TabSelection::Live(second.clone()))
        );
        // Next again wraps back to the first; Previous returns to the second.
        let _ = runtime.select_tab(TabDirection::Next);
        assert_eq!(
            runtime.active_pane().selected(),
            &PaneSelection::Tab(TabSelection::Live(first))
        );
        let _ = runtime.select_tab(TabDirection::Previous);
        assert_eq!(
            runtime.active_pane().selected(),
            &PaneSelection::Tab(TabSelection::Live(second))
        );
    }

    #[test]
    fn select_tab_from_target_selection_picks_the_edge_tab() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let first_op = OperationId::new();
        let first = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, first_op, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, first_op, first.clone());
        let second_op = OperationId::new();
        let second = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, second_op, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, second_op, second.clone());

        let reset_to_target = |runtime: &mut WorkspaceRuntime| {
            let _ = reduce_registry(
                runtime_panes_mut(runtime),
                PaneRegistryEvent::Pane {
                    target,
                    event: PaneEvent::Select(PaneSelection::Target(target)),
                },
            );
        };

        // From a target selection, Next picks the first tab and Previous the last.
        reset_to_target(&mut runtime);
        let _ = runtime.select_tab(TabDirection::Next);
        assert_eq!(
            runtime.active_pane().selected(),
            &PaneSelection::Tab(TabSelection::Live(first))
        );
        reset_to_target(&mut runtime);
        let _ = runtime.select_tab(TabDirection::Previous);
        assert_eq!(
            runtime.active_pane().selected(),
            &PaneSelection::Tab(TabSelection::Live(second))
        );
    }

    #[test]
    fn select_tab_without_tabs_is_a_no_op() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        assert!(runtime.select_tab(TabDirection::Next).is_empty());
        assert!(runtime.select_tab(TabDirection::Previous).is_empty());
    }

    #[test]
    fn close_focused_pane_on_a_live_tab_detaches_and_drops_it() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let operation = OperationId::new();
        let terminal = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, operation, terminal.clone());
        let _ = runtime.focus_terminal(target, terminal.clone());
        assert!(runtime.state().has_live_pane());

        // Closing the focused live tab tells the shell to detach its subscription
        // and removes the tab so no live pane remains.
        let outcome = runtime.close_focused_pane();
        assert_eq!(
            outcome,
            CloseOutcome {
                detach: Some(terminal),
                cancel: None,
            }
        );
        assert!(runtime.active_pane().tabs().is_empty());
        assert!(!runtime.state().has_live_pane());
    }

    #[test]
    fn close_focused_pane_on_a_pending_tab_cancels_its_launch() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let operation = OperationId::new();
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        // Select the pending placeholder, then close it.
        let _ = runtime.select_tab(TabDirection::Next);
        let outcome = runtime.close_focused_pane();
        assert_eq!(
            outcome,
            CloseOutcome {
                detach: None,
                cancel: Some(operation),
            }
        );
        assert!(runtime.active_pane().tabs().is_empty());
    }

    #[test]
    fn close_focused_pane_without_a_selected_tab_is_a_no_op() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        assert_eq!(runtime.close_focused_pane(), CloseOutcome::default());
        assert!(runtime.active_pane().tabs().is_empty());
    }

    #[test]
    fn on_effect_never_records_a_terminal_placeholder_for_the_root() {
        let workspace = WorkspaceId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        assert_eq!(runtime.state().active(), Target::Root(workspace));
        runtime.on_effect(&Effect::OpenTerminal {
            target: Target::Root(workspace),
            operation_id: OperationId::new(),
            arguments: String::new(),
        });
        // The root has no pane strip, so the request is dropped instead of
        // stranding a placeholder that no completion can promote.
        assert!(runtime.active_pane().tabs().is_empty());
    }

    #[test]
    fn live_action_keys_and_events_reduce_through_the_runtime() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        // A resolved Ctrl-O switch returns Closeup to Switch.
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::Switch));
        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Switch)
        ));
        // Backend/tick events flow through apply_event.
        let _ = runtime.apply_event(AppEvent::Tick);
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Down));
    }

    #[test]
    fn on_effect_mirrors_pane_effects_and_ignores_the_rest() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);

        // OpenTerminal / LaunchAgent record pending placeholders for their target.
        runtime.on_effect(&Effect::OpenTerminal {
            target: Target::Session(session),
            operation_id: OperationId::new(),
            arguments: String::new(),
        });
        assert!(matches!(
            runtime.active_pane().tabs().last(),
            Some(PaneTab::Pending(pending)) if pending.kind == PaneKind::Terminal
        ));
        let agent_op = OperationId::new();
        runtime.on_effect(&Effect::LaunchAgent {
            workspace,
            session,
            operation_id: agent_op,
            profile: None,
        });
        assert!(matches!(
            runtime.active_pane().tabs().last(),
            Some(PaneTab::Pending(pending)) if pending.kind == PaneKind::Agent
        ));

        // A non-pane effect leaves the tabs untouched.
        let before = runtime.active_pane().tabs().len();
        runtime.on_effect(&Effect::RefreshSessions { workspace });
        assert_eq!(runtime.active_pane().tabs().len(), before);

        // SelectTab routes through the tab cycler.
        runtime.on_effect(&Effect::SelectTab {
            direction: TabDirection::Previous,
        });
    }

    #[test]
    fn render_draws_the_controller_home_frame() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let projected = crate::presentation::views::workspace::ProjectedSession {
            id: session,
            label: "alpha".to_owned(),
            detail: "fixture".to_owned(),
            cwd: "/work/alpha".into(),
            last_modified: Utc::now(),
            has_notes: false,
            pr_summary: None,
        };
        let frame = runtime.render(
            20,
            80,
            "atlas",
            "/work/root",
            std::slice::from_ref(&projected),
            None,
            &BTreeMap::new(),
            None,
        );
        let text = frame.join("\n");
        assert!(text.contains("atlas"));
        assert!(text.contains("alpha"));
        assert!(text.contains("+ new session"));
    }

    #[test]
    fn tab_selection_maps_every_tab_kind() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        let pending = PendingPane {
            operation,
            target: Target::Session(session),
            kind: PaneKind::Diff,
        };
        assert_eq!(
            tab_selection(&PaneTab::Pending(pending)),
            TabSelection::Pending(operation)
        );
        assert_eq!(
            tab_selection(&PaneTab::Ready(pending)),
            TabSelection::Ready(operation)
        );
        let terminal = terminal_ref(workspace, session);
        assert_eq!(
            tab_selection(&PaneTab::Live(LivePane {
                terminal: terminal.clone(),
                kind: PaneKind::Terminal,
            })),
            TabSelection::Live(terminal)
        );
    }

    fn runtime_panes_mut(runtime: &mut WorkspaceRuntime) -> &mut PaneRegistry {
        &mut runtime.panes
    }
}

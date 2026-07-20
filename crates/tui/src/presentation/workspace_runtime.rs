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

use crate::presentation::views::closeup_modal::CloseupModal;
use crate::presentation::views::overview_modal::OverviewModal;
use crate::presentation::views::workspace::{
    GitDiff, HomeProjection, ProjectedSession, TerminalViewProjection, render_home,
};
use crate::usecase::application::Key;
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, Effect, HomeMode, Overlay, Route, TabDirection, Target, update,
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
    /// Persisted input state for the Overview (`:`) command palette. Present only
    /// while the controller's [`Overlay::Overview`] is open, so its caret,
    /// filter, and history survive across frames instead of being rebuilt.
    overview_modal: Option<OverviewModal>,
    /// Persisted input state for the Closeup action modal. Present only while the
    /// controller's [`Overlay::Closeup`] is open.
    closeup_modal: Option<CloseupModal>,
    /// User-interaction count captured when each pane launch was requested. A
    /// completion may focus its tab only while the count is unchanged, mirroring
    /// the controller's create-session gate
    /// ([`AppState::interaction_count`]/[`PendingOperation::interaction_at_accept`]).
    /// The entry is dropped when the launch completes, fails, or is cancelled.
    pane_focus_at_request: BTreeMap<OperationId, u64>,
}

impl WorkspaceRuntime {
    /// Start a Home runtime for `workspace` with the daemon-authoritative
    /// `sessions`. The initial selection and active target are the workspace
    /// root, matching [`AppState::home`].
    #[must_use]
    pub fn new(workspace: usagi_core::domain::id::WorkspaceId, sessions: Vec<SessionId>) -> Self {
        let state = AppState::home(workspace, sessions);
        let panes = PaneRegistry::new(state.active());
        Self {
            state,
            panes,
            overview_modal: None,
            closeup_modal: None,
            pane_focus_at_request: BTreeMap::new(),
        }
    }

    /// The Overview command palette's persisted input state, if its overlay is
    /// open. The shell renders it instead of rebuilding an empty palette.
    #[must_use]
    pub const fn overview_modal(&self) -> Option<&OverviewModal> {
        self.overview_modal.as_ref()
    }

    /// The Closeup action modal's persisted input state, if its overlay is open.
    #[must_use]
    pub const fn closeup_modal(&self) -> Option<&CloseupModal> {
        self.closeup_modal.as_ref()
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
        // The Overview / Closeup overlays own keyboard input while open: their
        // persisted modal edits its own caret and selection, and the sidebar
        // reducer never sees the key. This is the symmetry the other overlays
        // already have, and it is why an open palette can no longer move the
        // hidden Home cursor.
        if self.state.overlay() == Some(Overlay::Overview) && self.overview_modal.is_some() {
            return self.handle_overview_key(key);
        }
        if self.state.overlay() == Some(Overlay::Closeup) && self.closeup_modal.is_some() {
            return self.handle_closeup_key(key);
        }
        match app_event_from_key(key) {
            Some(event) => self.apply_event(event),
            None => Vec::new(),
        }
    }

    /// Drive the Overview palette from one terminal key. Editing keys mutate the
    /// persisted modal in place; Enter submits its resolved command through the
    /// reducer as [`AppKey::SubmitOverview`], and Escape closes the overlay.
    /// Every other key falls through to the reducer so global chords still work.
    fn handle_overview_key(&mut self, key: Key) -> Vec<Effect> {
        let modal = self
            .overview_modal
            .as_mut()
            .expect("overview modal present when Overview overlay is open");
        match key {
            Key::Up => {
                if !modal.recall_previous() {
                    modal.select_prev();
                }
                Vec::new()
            }
            Key::Down => {
                if !modal.recall_next() {
                    modal.select_next();
                }
                Vec::new()
            }
            Key::Left => {
                modal.cursor_left();
                Vec::new()
            }
            Key::Right => {
                modal.cursor_right();
                Vec::new()
            }
            Key::Backspace => {
                modal.backspace();
                Vec::new()
            }
            Key::Tab => {
                modal.complete_selected();
                Vec::new()
            }
            Key::Char(character) => {
                modal.insert_char(character);
                Vec::new()
            }
            Key::Enter => {
                let submission = modal.submission();
                modal.record_submission();
                self.apply_event(AppEvent::Key(AppKey::SubmitOverview(submission)))
            }
            Key::Escape => self.apply_event(AppEvent::Key(AppKey::Escape)),
            other => match app_event_from_key(other) {
                Some(event) => self.apply_event(event),
                None => Vec::new(),
            },
        }
    }

    /// Drive the Closeup action modal from one terminal key. Editing keys mutate
    /// the persisted modal; Enter submits the selected action or typed command as
    /// [`AppKey::SubmitCloseup`], and Escape closes the overlay.
    fn handle_closeup_key(&mut self, key: Key) -> Vec<Effect> {
        let modal = self
            .closeup_modal
            .as_mut()
            .expect("closeup modal present when Closeup overlay is open");
        match key {
            Key::Up => {
                modal.select_prev();
                Vec::new()
            }
            Key::Down => {
                modal.select_next();
                Vec::new()
            }
            Key::Left => {
                modal.collapse();
                Vec::new()
            }
            Key::Right => {
                modal.expand_selected();
                Vec::new()
            }
            Key::Backspace => {
                modal.backspace();
                Vec::new()
            }
            Key::Tab => {
                modal.complete_selected();
                Vec::new()
            }
            Key::Char(character) => {
                modal.insert_char(character);
                Vec::new()
            }
            Key::Enter => {
                let submission = modal.submission();
                self.apply_event(AppEvent::Key(AppKey::SubmitCloseup(submission)))
            }
            Key::Escape => self.apply_event(AppEvent::Key(AppKey::Escape)),
            other => match app_event_from_key(other) {
                Some(event) => self.apply_event(event),
                None => Vec::new(),
            },
        }
    }

    /// Reduce one [`AppEvent`] (key, resize, tick, backend, completion) and
    /// return its effects, keeping the pane registry's active target and the
    /// live-pane flag in sync with the resulting controller state.
    #[must_use]
    pub fn apply_event(&mut self, event: AppEvent) -> Vec<Effect> {
        let effects = update(&mut self.state, event);
        self.follow_active_target();
        self.sync_overlay_modals();
        effects
    }

    /// Keep the persisted Overview / Closeup modals aligned with the controller's
    /// overlay state. Opening an overlay lazily creates its empty modal; closing
    /// it (through submit, Escape, or a live-pane transition) drops the modal so
    /// its caret and filter never leak into the next time it opens.
    fn sync_overlay_modals(&mut self) {
        if self.state.overlay() == Some(Overlay::Overview) {
            self.overview_modal.get_or_insert_with(OverviewModal::new);
        } else {
            self.overview_modal = None;
        }
        if self.state.overlay() == Some(Overlay::Closeup) {
            self.closeup_modal
                .get_or_insert_with(|| CloseupModal::new(String::new()));
        } else {
            self.closeup_modal = None;
        }
    }

    /// Whether a live terminal currently owns keyboard input, so the shell
    /// forwards raw passthrough bytes to the PTY instead of the reducer. True
    /// only in Closeup with an available live pane whose tab (not the action
    /// modal) owns input.
    #[must_use]
    pub fn wants_live_input(&self) -> bool {
        matches!(self.state.route(), Route::Home(HomeMode::Closeup))
            && self.state.has_live_pane()
            && self.state.overlay().is_none()
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
    ///
    /// The current interaction count is captured so a later
    /// [`Self::complete_pane_focus_if_uninterrupted`] only steals focus when the
    /// user has not touched the UI since the launch was accepted.
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
        self.pane_focus_at_request
            .insert(operation, self.state.interaction_count());
        self.sync_live_pane();
        effects
    }

    /// Promote a pending placeholder to a live tab once the daemon confirms the
    /// terminal identity, then focus it only when no user interaction has
    /// happened since the launch was requested.
    ///
    /// This is the shell's single entry point for a daemon completion: the focus
    /// decision lives here (via the captured interaction count) rather than
    /// leaking a condition into the frame loop, matching the create-session gate.
    /// Completion always promotes the tab; only the focus is gated.
    pub fn complete_pane_focus_if_uninterrupted(
        &mut self,
        target: Target,
        operation: OperationId,
        terminal: TerminalRef,
    ) -> Vec<PaneRegistryEffect> {
        let accepted_at = self.pane_focus_at_request.remove(&operation);
        let mut effects = self.complete_pane(target, operation, terminal.clone());
        if accepted_at == Some(self.state.interaction_count()) {
            effects.extend(self.focus_terminal(target, terminal));
        }
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
        // A dropped placeholder can never complete, so retire its focus gate.
        self.pane_focus_at_request.remove(&operation);
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
        // A cancelled pending launch will never complete, so drop its focus gate
        // before the placeholder leaves the registry.
        if let Some(operation) = outcome.cancel {
            self.pane_focus_at_request.remove(&operation);
        }
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
            // A terminal opens against any target's pane strip, including the
            // workspace root (`Target::Root`); the daemon resolves the root
            // scope to the trusted repository root.
            Effect::OpenTerminal {
                target,
                operation_id,
                ..
            } => {
                let _ = self.request_pane(*target, *operation_id, PaneKind::Terminal);
            }
            Effect::LaunchAgent {
                workspace,
                session,
                operation_id,
                ..
            } => {
                let target = session.map_or(Target::Root(*workspace), Target::Session);
                let _ = self.request_pane(target, *operation_id, PaneKind::Agent);
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

    /// Sample the active target's live-pane availability into the controller.
    /// This runs after every event and pane transition, so it feeds the reducer
    /// the current *level*; the reducer detects the edge and stays inert on an
    /// unchanged level (see [`AppEvent::LivePaneAvailability`]). That keeps an
    /// overlay opened in the same batch (quit confirmation, PR / Preview) and
    /// the Ctrl-C grace from being clobbered by the next sample.
    fn sync_live_pane(&mut self) {
        // Any active target with a live tab — a session or the workspace root —
        // carries the live signal; the pane registry is keyed uniformly.
        let live = self
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
                .with_terminal_view(terminal_view)
                .with_overlay_modals(self.overview_modal.clone(), self.closeup_modal.clone());
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
        AppEvent, AppKey, Effect, HomeMode, Overlay, Route, Selection, TabDirection, Target,
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

    /// Open the Overview palette from a fresh runtime and confirm its persisted
    /// modal exists.
    fn overview_on(workspace: WorkspaceId) -> WorkspaceRuntime {
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        let _ = runtime.handle_key(Key::Char(':'));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Overview));
        assert!(runtime.overview_modal().is_some());
        runtime
    }

    fn type_str(runtime: &mut WorkspaceRuntime, text: &str) {
        for character in text.chars() {
            let _ = runtime.handle_key(Key::Char(character));
        }
    }

    #[test]
    fn overview_palette_runs_a_typed_command_through_the_reducer() {
        let workspace = WorkspaceId::new();
        let mut runtime = overview_on(workspace);
        // Typing edits the palette, not the hidden sidebar cursor.
        let before = runtime.state().selected();
        let _ = runtime.handle_key(Key::Down); // candidate move, not sidebar move
        type_str(&mut runtime, "session list");
        assert_eq!(runtime.state().selected(), before);
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::RefreshSessions { .. })),
            "{effects:?}"
        );
        // A submitted command closes the palette and drops its modal.
        assert_eq!(runtime.state().overlay(), None);
        assert!(runtime.overview_modal().is_none());
    }

    #[test]
    fn overview_palette_creates_a_session() {
        let workspace = WorkspaceId::new();
        let mut runtime = overview_on(workspace);
        type_str(&mut runtime, "session create feature-x");
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CreateSession { .. })),
            "{effects:?}"
        );
    }

    #[test]
    fn overview_editing_keys_move_the_caret_and_filter_without_touching_the_sidebar() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let _ = runtime.handle_key(Key::Char(':'));
        let before = runtime.state().selected();
        type_str(&mut runtime, "issue");
        let _ = runtime.handle_key(Key::Backspace); // "issu"
        let _ = runtime.handle_key(Key::Left); // caret motion
        let _ = runtime.handle_key(Key::Right);
        let _ = runtime.handle_key(Key::Tab); // complete → "issue"
        let _ = runtime.handle_key(Key::Up); // no history yet → candidate move
        let _ = runtime.handle_key(Key::Down);
        assert_eq!(runtime.state().selected(), before);
        assert_eq!(runtime.overview_modal().unwrap().input(), "issue");
    }

    #[test]
    fn overview_history_recall_survives_an_invalid_submit() {
        let workspace = WorkspaceId::new();
        let mut runtime = overview_on(workspace);
        // An unknown command keeps the palette open and records the submission.
        type_str(&mut runtime, "zzz");
        let _ = runtime.handle_key(Key::Enter);
        assert_eq!(runtime.state().overlay(), Some(Overlay::Overview));
        // Clear the draft, then Up recalls the recorded command.
        for _ in 0..3 {
            let _ = runtime.handle_key(Key::Backspace);
        }
        let _ = runtime.handle_key(Key::Up);
        assert_eq!(runtime.overview_modal().unwrap().input(), "zzz");
        // Down walks history forward again.
        let _ = runtime.handle_key(Key::Down);
    }

    #[test]
    fn overview_escape_closes_the_palette() {
        let workspace = WorkspaceId::new();
        let mut runtime = overview_on(workspace);
        let effects = runtime.handle_key(Key::Escape);
        assert!(effects.is_empty());
        assert_eq!(runtime.state().overlay(), None);
        assert!(runtime.overview_modal().is_none());
    }

    #[test]
    fn overview_reserved_keys_fall_through_to_the_reducer() {
        let workspace = WorkspaceId::new();
        let mut runtime = overview_on(workspace);
        // Ctrl-C is swallowed by the open overlay; passthrough yields nothing.
        assert!(runtime.handle_key(Key::Quit).is_empty());
        assert!(runtime.handle_key(Key::Passthrough(vec![0x1b])).is_empty());
        assert_eq!(runtime.state().overlay(), Some(Overlay::Overview));
    }

    #[test]
    fn closeup_modal_launches_an_agent_by_default() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
        assert!(runtime.closeup_modal().is_some());
        // The default action is `agent`; Enter launches it for the active session.
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::LaunchAgent { .. })),
            "{effects:?}"
        );
        // Submitting closes the action modal; the edge-triggered live-pane level
        // no longer re-opens it while the launched pane is still pending.
        assert_eq!(runtime.state().overlay(), None);
        assert!(runtime.closeup_modal().is_none());
    }

    #[test]
    fn closeup_modal_opens_a_terminal_and_closes_a_session() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();

        // `terminal` is the last action; Up wraps to it.
        let mut runtime = closeup_on(workspace, session);
        let _ = runtime.handle_key(Key::Up);
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::OpenTerminal { .. })),
            "{effects:?}"
        );

        // `close` is the second action; Down selects it and submits a remove.
        let mut runtime = closeup_on(workspace, session);
        let _ = runtime.handle_key(Key::Down);
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::RemoveSession { .. })),
            "{effects:?}"
        );
    }

    #[test]
    fn closeup_editing_keys_drive_the_modal_without_moving_the_sidebar() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        let selected_row = runtime.state().selected();
        // Typing filters the action list; expand/collapse and completion edit the
        // modal in place.
        let _ = runtime.handle_key(Key::Down); // close
        let _ = runtime.handle_key(Key::Right); // expand subcommands
        let _ = runtime.handle_key(Key::Left); // collapse
        type_str(&mut runtime, "ter");
        let _ = runtime.handle_key(Key::Tab); // complete → terminal
        let _ = runtime.handle_key(Key::Backspace);
        assert!(matches!(runtime.state().selected(), Selection::Target(_)));
        assert_eq!(runtime.state().selected(), selected_row);
        // The modal persists across the edits (no live pane keeps it the surface).
        assert!(runtime.closeup_modal().is_some());
        let effects = runtime.handle_key(Key::Escape);
        assert!(effects.is_empty());
    }

    #[test]
    fn closeup_reserved_keys_fall_through_to_the_reducer() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        // Ctrl-Q stays swallowed by the open overlay and raw passthrough is inert;
        // neither leaves the Closeup action modal (unlike Escape / Ctrl-C).
        assert!(runtime.handle_key(Key::CtrlQ).is_empty());
        assert!(runtime.handle_key(Key::Passthrough(vec![0x1b])).is_empty());
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
    }

    /// #355: the real-loop key translation exits the Closeup action modal to
    /// Switch on both Escape and Ctrl-C (`Key::Quit`), dropping the persisted
    /// modal so its caret never leaks into the next open.
    #[test]
    fn closeup_modal_escape_and_ctrl_c_exit_to_switch() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        for exit_key in [Key::Escape, Key::Quit] {
            let mut runtime = closeup_on(workspace, session);
            assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
            assert!(runtime.closeup_modal().is_some());
            let effects = runtime.handle_key(exit_key.clone());
            assert!(effects.is_empty(), "{exit_key:?}");
            assert!(
                matches!(runtime.state().route(), Route::Home(HomeMode::Switch)),
                "{exit_key:?}"
            );
            assert_eq!(runtime.state().overlay(), None, "{exit_key:?}");
            assert!(runtime.closeup_modal().is_none(), "{exit_key:?}");
        }
    }

    #[test]
    fn open_action_overlay_disarms_live_passthrough() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let operation = OperationId::new();
        let terminal = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, operation, terminal.clone());
        let _ = runtime.focus_terminal(target, terminal);
        // A focused live pane owns input until the action modal opens over it.
        assert!(runtime.wants_live_input());
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::OpenCloseupModal));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
        assert!(runtime.closeup_modal().is_some());
        assert!(!runtime.wants_live_input());
        // #355: Escape dismisses the forced modal and leaves Closeup for Switch
        // (rather than handing input back to the live pane), so live passthrough
        // stays disarmed until the session is re-activated.
        let _ = runtime.handle_key(Key::Escape);
        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Switch)
        ));
        assert_eq!(runtime.state().overlay(), None);
        assert!(runtime.closeup_modal().is_none());
        assert!(!runtime.wants_live_input());
    }

    #[test]
    fn render_draws_the_open_overview_palette() {
        let workspace = WorkspaceId::new();
        let mut runtime = overview_on(workspace);
        type_str(&mut runtime, "session");
        let frame = runtime.render(
            24,
            80,
            "atlas",
            "/work/root",
            &[],
            None,
            &BTreeMap::new(),
            None,
        );
        assert!(frame.join("\n").contains("Overview"));
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
        // With no overlay open, a key the Home reducer never consumes (raw
        // passthrough) is dropped before the reducer, and a key it consumes but
        // ignores here (Left, which only moves the Yes/No quit focus) is inert.
        assert!(runtime.handle_key(Key::Passthrough(vec![0x1b])).is_empty());
        assert!(runtime.handle_key(Key::Left).is_empty());
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
    fn on_effect_records_a_terminal_placeholder_for_the_root() {
        let workspace = WorkspaceId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        assert_eq!(runtime.state().active(), Target::Root(workspace));
        runtime.on_effect(&Effect::OpenTerminal {
            target: Target::Root(workspace),
            operation_id: OperationId::new(),
            arguments: String::new(),
        });
        // The workspace root owns a pane strip like any target: the request
        // records a pending placeholder the daemon completion later promotes.
        assert!(matches!(
            runtime.active_pane().tabs().last(),
            Some(PaneTab::Pending(pending)) if pending.kind == PaneKind::Terminal
        ));
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
            session: Some(session),
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
            removing: false,
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

    /// #352 regression: a Closeup quit confirmation over a live pane must
    /// survive the same-batch live resample `apply_event` performs, otherwise
    /// the only quit path is switching away.
    #[test]
    fn closeup_live_quit_confirmation_survives_live_resample() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);

        // Arm a live pane so Ctrl-C opens the quit confirmation.
        let operation = OperationId::new();
        let terminal = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, operation, terminal);
        assert!(runtime.state().has_live_pane());
        assert_eq!(runtime.state().overlay(), None);

        // Ctrl-C opens the confirmation and the trailing live resample keeps it.
        let _ = runtime.apply_event(AppEvent::Key(AppKey::CtrlC));
        assert_eq!(runtime.state().overlay(), Some(Overlay::QuitConfirmation));

        // It stays operable: a tick resamples live yet keeps it, then 'n' cancels.
        let _ = runtime.apply_event(AppEvent::Tick);
        assert_eq!(runtime.state().overlay(), Some(Overlay::QuitConfirmation));
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Char('n')));
        assert_eq!(runtime.state().overlay(), None);

        // Reopening and confirming quit still reaches Detach.
        let _ = runtime.apply_event(AppEvent::Key(AppKey::CtrlC));
        let effects = runtime.apply_event(AppEvent::Key(AppKey::Enter));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Detach))
        );
    }

    /// #352 regression: PR / Preview overlays opened in a non-live Closeup must
    /// not be overwritten by the same-batch (or a later tick's) live resample,
    /// which previously forced `Overlay::Closeup` and stuck the modal.
    #[test]
    fn nonlive_closeup_pr_and_preview_overlays_open_and_close() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        assert!(!runtime.state().has_live_pane());

        // The PR overlay opens and stays open across a resampling tick.
        let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenPrs));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));
        let _ = runtime.apply_event(AppEvent::Tick);
        assert_eq!(runtime.state().overlay(), Some(Overlay::Prs));
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));
        assert_eq!(runtime.state().overlay(), None);

        // The Preview overlay behaves the same way.
        let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenPreview));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Preview));
        let _ = runtime.apply_event(AppEvent::Tick);
        assert_eq!(runtime.state().overlay(), Some(Overlay::Preview));
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));
        assert_eq!(runtime.state().overlay(), None);
    }

    /// #352 regression: the Ctrl-C grace armed by leaving a live pane must not
    /// be cleared by the next tick's resample of the unchanged non-live level.
    #[test]
    fn ctrl_c_grace_survives_a_tick_resample() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);

        // Establish then drop a live pane: leaving it arms the Ctrl-C grace.
        let operation = OperationId::new();
        let terminal = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, operation, PaneKind::Terminal);
        let _ = runtime.complete_pane(target, operation, terminal.clone());
        assert!(runtime.state().has_live_pane());
        let _ = runtime.exit_pane(target, terminal);
        assert!(!runtime.state().has_live_pane());
        assert!(runtime.state().ctrl_c_grace());

        // A tick resamples the unchanged non-live level and must keep the grace.
        let _ = runtime.apply_event(AppEvent::Tick);
        assert!(runtime.state().ctrl_c_grace());
    }

    fn runtime_panes_mut(runtime: &mut WorkspaceRuntime) -> &mut PaneRegistry {
        &mut runtime.panes
    }

    fn strip(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    /// Render one Home frame through the runtime and flatten it to plain text.
    fn joined_frame(runtime: &WorkspaceRuntime) -> String {
        runtime
            .render(24, 100, "work", "/work", &[], None, &BTreeMap::new(), None)
            .iter()
            .map(|line| strip(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Submit the default `agent` action from an open Closeup modal and mirror
    /// the resulting launch into the pane registry as the shell would, returning
    /// the launch operation id.
    fn submit_agent(runtime: &mut WorkspaceRuntime) -> OperationId {
        let effects = runtime.handle_key(Key::Enter);
        let mut operation = None;
        for effect in &effects {
            if let Effect::LaunchAgent { operation_id, .. } = effect {
                operation = Some(*operation_id);
            }
            runtime.on_effect(effect);
        }
        operation.expect("Enter submits a LaunchAgent effect")
    }

    // ── R1: pending must not be re-covered by the action modal ───────────────

    #[test]
    fn pending_agent_launch_is_not_covered_by_the_action_modal() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        // Entering Closeup with an empty pane shows the action launcher.
        assert!(
            joined_frame(&runtime).contains("Closeup:"),
            "the launcher is shown while the pane is empty"
        );

        let _ = submit_agent(&mut runtime);
        // Submitting closes the modal and leaves a pending Agent tab. The action
        // modal must not re-open over it every frame (the R1 regression); the
        // pending tab and its wave own the pane.
        assert_eq!(runtime.state().overlay(), None);
        let frame = joined_frame(&runtime);
        assert!(
            !frame.contains("Closeup:"),
            "the pending wave must not be covered by the action modal: {frame}"
        );
        assert!(frame.contains("Agent"), "the pending Agent tab is listed");
    }

    #[test]
    fn failed_launch_restores_the_action_launcher() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        let operation = submit_agent(&mut runtime);
        assert!(!joined_frame(&runtime).contains("Closeup:"));
        // A failed launch drops the pending tab, so the launcher returns for the
        // now-empty pane.
        let _ = runtime.fail_pane(Target::Session(session), operation, "boom".to_owned());
        assert!(runtime.active_pane().tabs().is_empty());
        assert!(
            joined_frame(&runtime).contains("Closeup:"),
            "the launcher returns once the pane is empty again"
        );
    }

    // ── R2: completion focus is gated on no later interaction ────────────────

    #[test]
    fn uninterrupted_completion_focuses_the_new_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let operation = submit_agent(&mut runtime);
        let terminal = terminal_ref(workspace, session);
        // No interaction between request and completion → focus the completed tab.
        let _ = runtime.complete_pane_focus_if_uninterrupted(target, operation, terminal.clone());
        assert_eq!(
            runtime.active_pane().selected(),
            &PaneSelection::Tab(TabSelection::Live(terminal))
        );
        assert!(runtime.state().has_live_pane());
    }

    #[test]
    fn interaction_after_launch_cancels_completion_focus() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let operation = submit_agent(&mut runtime);
        // The user navigates while the pane loads. A late completion still
        // promotes the tab but must not steal focus into it.
        let _ = runtime.handle_key(Key::Down);
        let terminal = terminal_ref(workspace, session);
        let _ = runtime.complete_pane_focus_if_uninterrupted(target, operation, terminal);
        // The tab is still promoted to live...
        let tabs = runtime.active_pane().tabs();
        assert_eq!(tabs.len(), 1);
        assert!(matches!(tabs[0], PaneTab::Live(_)));
        // ...but the completion did not steal focus into it: the selection stays
        // off the freshly live tab, so no live terminal is focused.
        assert!(runtime.focused_terminal().is_none());
    }
}

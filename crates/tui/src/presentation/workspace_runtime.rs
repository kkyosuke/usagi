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

use usagi_core::domain::agent::AgentResumeRelation;
use usagi_core::domain::id::AgentContinuationRef;
use usagi_core::domain::id::{OperationId, SessionId, TerminalRef};
use usagi_core::domain::settings::{AvailableModels, DefaultModel, ModalSelectionMode};
use usagi_core::usecase::client::DaemonMetrics;

use crate::presentation::views::closeup_modal::CloseupModal;
use crate::presentation::views::overview_modal::OverviewModal;
use crate::presentation::views::workspace::{
    GitDiff, HomeProjection, ProjectedSession, TerminalViewProjection, render_home,
};
use crate::presentation::views::workspace_agent_drawer::WorkspaceAgentDrawerProjection;
use crate::usecase::application::Key;
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, Effect, HomeMode, Overlay, Route, TabDirection, Target, update,
};
use crate::usecase::application::interrupted_tab::{
    InterruptedTab, ResumeCommand, ResumeRejection, accept_replacement, resume_command,
};
use crate::usecase::application::pane::{
    InterruptedPane, LivePane, PaneEvent, PaneInputOwner, PaneKind, PaneRegistry,
    PaneRegistryEffect, PaneRegistryEvent, PaneSelection, PaneState, PaneTab, PaneTabCommand,
    TabSelection, reduce_registry, route_tab_command,
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

/// One safe choice shown by `Reopen closed Agent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReopenChoice {
    pub label: String,
    pub continuation: AgentContinuationRef,
}

/// One target's ordered live panes from a completed restore job, plus the
/// interrupted Agent conversations projected for the same target (#510).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRestoreTarget {
    pub target: Target,
    pub panes: Vec<LivePane>,
    pub selected: Option<TerminalRef>,
    /// Saved interrupted selection for this target. It is applied only after
    /// the interrupted inventory has been restored, and never starts resume.
    pub selected_interrupted: Option<AgentContinuationRef>,
    /// Interrupted conversations of this target, in display order. They are
    /// read-only tabs until the user explicitly resumes one.
    pub interrupted: Vec<InterruptedTab>,
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
    modal_selection_mode: ModalSelectionMode,
    /// User-interaction count captured when each pane launch was requested. A
    /// completion may focus its tab only while the count is unchanged, mirroring
    /// the controller's create-session gate
    /// ([`AppState::interaction_count`]/[`PendingOperation::interaction_at_accept`]).
    /// The entry is dropped when the launch completes, fails, or is cancelled.
    pane_focus_at_request: BTreeMap<OperationId, u64>,
    reopen_choices: Vec<AgentReopenChoice>,
    workspace_agent_projection: WorkspaceAgentDrawerProjection,
}

impl WorkspaceRuntime {
    /// Start a Home runtime for `workspace` with the daemon-authoritative
    /// `sessions`. The first managed session is active when present; an empty
    /// snapshot has no active pane target and never falls back to workspace root.
    #[must_use]
    pub fn new(workspace: usagi_core::domain::id::WorkspaceId, sessions: Vec<SessionId>) -> Self {
        Self::with_selection_mode(workspace, sessions, ModalSelectionMode::Action)
    }

    /// Start a Home runtime using the effective settings resolved for this
    /// workspace entry.
    #[must_use]
    pub fn with_selection_mode(
        workspace: usagi_core::domain::id::WorkspaceId,
        sessions: Vec<SessionId>,
        modal_selection_mode: ModalSelectionMode,
    ) -> Self {
        let state = AppState::home(workspace, sessions);
        let panes = PaneRegistry::new(state.active().map(Target::Session));
        Self {
            state,
            panes,
            overview_modal: None,
            closeup_modal: None,
            modal_selection_mode,
            pane_focus_at_request: BTreeMap::new(),
            reopen_choices: Vec::new(),
            workspace_agent_projection: WorkspaceAgentDrawerProjection::default(),
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

    /// Apply a newly saved workspace setting to future Overview / Closeup
    /// palettes without rebuilding the workspace runtime or its live panes.
    pub fn set_modal_selection_mode(&mut self, mode: ModalSelectionMode) {
        self.modal_selection_mode = mode;
    }

    /// Apply the observed Agent CLI availability and the configured default
    /// provider. An open Closeup modal is re-projected so its `agent -m` picker
    /// and completion follow a newly saved setting immediately.
    pub fn set_agent_models(&mut self, available: AvailableModels, default: DefaultModel) {
        self.state.set_agent_models(available, default);
        if let Some(modal) = self.closeup_modal.take() {
            self.closeup_modal = Some(modal.with_agent_models(available, default));
        }
    }

    /// Capture both fences carried by an off-thread restore dispatch.
    #[must_use]
    pub const fn restore_fence(&self) -> (u64, u64) {
        (self.state.interaction_count(), self.panes.revision())
    }

    /// Replace the secret-free list used by the Closeup reopen picker.
    pub fn set_reopen_choices(&mut self, choices: Vec<AgentReopenChoice>) {
        self.reopen_choices = choices;
        if let Some(modal) = self.closeup_modal.take() {
            self.closeup_modal = Some(modal.with_reopen_choices(self.reopen_choices.clone()));
        }
    }

    /// Replace presentation-only material for the open root Agent drawer.
    pub fn set_workspace_agent_projection(&mut self, projection: WorkspaceAgentDrawerProjection) {
        self.workspace_agent_projection = projection;
    }

    #[must_use]
    pub const fn workspace_agent_projection(&self) -> &WorkspaceAgentDrawerProjection {
        &self.workspace_agent_projection
    }

    /// Apply one completed inventory projection. Only a result matching both
    /// dispatch fences may change the registry. A late result is rejected in
    /// full so it cannot append a tab or overwrite later UI intent.
    #[must_use]
    pub fn restore_snapshot(
        &mut self,
        dispatched_interaction: u64,
        dispatched_registry_revision: u64,
        targets: Vec<PaneRestoreTarget>,
    ) -> bool {
        self.apply_restore_snapshot(
            dispatched_interaction,
            dispatched_registry_revision,
            targets,
            true,
        )
    }

    /// Append inventory panes under the same interaction/revision fence while
    /// preserving every existing tab, order, and selection. This conservative
    /// path is used only when Agent intent persistence failed: generic panes
    /// remain available without treating uncommitted Agent inventory as UI
    /// intent or destructively applying a partial projection.
    #[must_use]
    pub fn append_restore_snapshot(
        &mut self,
        dispatched_interaction: u64,
        dispatched_registry_revision: u64,
        targets: Vec<PaneRestoreTarget>,
    ) -> bool {
        self.apply_restore_snapshot(
            dispatched_interaction,
            dispatched_registry_revision,
            targets,
            false,
        )
    }

    fn apply_restore_snapshot(
        &mut self,
        dispatched_interaction: u64,
        dispatched_registry_revision: u64,
        targets: Vec<PaneRestoreTarget>,
        replace_order: bool,
    ) -> bool {
        if self.restore_fence() != (dispatched_interaction, dispatched_registry_revision) {
            return false;
        }
        for target in targets {
            let entry = target.target;
            let root = matches!(entry, Target::Root(_));
            let panes = if root {
                target
                    .panes
                    .into_iter()
                    .filter(|pane| pane.kind == PaneKind::Agent)
                    .collect()
            } else {
                target.panes
            };
            let _ = reduce_registry(
                &mut self.panes,
                PaneRegistryEvent::Pane {
                    target: entry,
                    event: PaneEvent::RestoreBatch {
                        panes,
                        selected: target.selected,
                        replace_order,
                    },
                },
            );
            // The interrupted projection is authoritative for history
            // membership, so it is merged only on the authoritative path and
            // after live membership: a history tab then keeps its slot behind
            // this target's live tabs. The append-only path preserves every
            // existing tab instead, since its observation could not be trusted
            // as display intent.
            if replace_order {
                let _ = reduce_registry(
                    &mut self.panes,
                    PaneRegistryEvent::Pane {
                        target: entry,
                        event: PaneEvent::RestoreInterrupted {
                            tabs: target.interrupted,
                        },
                    },
                );
                if let Some(continuation) = target.selected_interrupted
                    && self.panes.pane(entry).is_some_and(|pane| {
                        pane.tabs().iter().any(|tab| {
                            matches!(
                                tab,
                                PaneTab::Interrupted(interrupted)
                                    if interrupted.tab.continuation == continuation
                            )
                        })
                    })
                {
                    let _ = reduce_registry(
                        &mut self.panes,
                        PaneRegistryEvent::Pane {
                            target: entry,
                            event: PaneEvent::Select(PaneSelection::Tab(
                                TabSelection::Interrupted(continuation),
                            )),
                        },
                    );
                }
            }
        }
        self.sync_live_pane();
        true
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
        // With no existing modal in front, the drawer owns every Home input.
        // Only Escape and the resolved `Ctrl-O g` toggle can close it; every
        // other key is consumed without reaching sidebar, pane, or globals.
        if self.state.workspace_agent_drawer_open() {
            return match key {
                Key::Escape => self.apply_event(AppEvent::Key(AppKey::Escape)),
                Key::Live(crate::usecase::terminal_input::LiveTerminalAction::WorkspaceAgent) => {
                    self.apply_event(AppEvent::Key(AppKey::ToggleWorkspaceAgentDrawer))
                }
                _ => Vec::new(),
            };
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
            // A focused palette input owns Home/End and emacs Ctrl-A/Ctrl-E as
            // caret motion, so they never reach the reducer's `+ new session`.
            Key::Home | Key::LineStart => {
                modal.cursor_home();
                Vec::new()
            }
            Key::End | Key::LineEnd => {
                modal.cursor_end();
                Vec::new()
            }
            Key::Delete => {
                modal.delete_forward();
                Vec::new()
            }
            Key::SelectLeft => {
                modal.select_left();
                Vec::new()
            }
            Key::SelectRight => {
                modal.select_right();
                Vec::new()
            }
            Key::SelectHome => {
                modal.select_home();
                Vec::new()
            }
            Key::SelectEnd => {
                modal.select_end();
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
            // Left/Right drive the action picker's collapse/expand here, so the
            // prompt caret uses Home/End and emacs Ctrl-A/Ctrl-E, and selection
            // extends with Shift+arrow / Shift+Home/End.
            Key::Home | Key::LineStart => {
                modal.cursor_home();
                Vec::new()
            }
            Key::End | Key::LineEnd => {
                modal.cursor_end();
                Vec::new()
            }
            Key::Delete => {
                modal.delete_forward();
                Vec::new()
            }
            Key::SelectLeft => {
                modal.select_left();
                Vec::new()
            }
            Key::SelectRight => {
                modal.select_right();
                Vec::new()
            }
            Key::SelectHome => {
                modal.select_home();
                Vec::new()
            }
            Key::SelectEnd => {
                modal.select_end();
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
                let effects = self.apply_event(AppEvent::Key(AppKey::SubmitCloseup(submission)));
                // An accepted command produces its effect and closes the overlay
                // (which drops the modal). A refused one produces nothing and
                // leaves the modal on screen, so carry the reducer's safe message
                // into it — otherwise the refusal looks like a dead key.
                if effects.is_empty() {
                    let message = self.state.notice().map(|notice| notice.message.clone());
                    if let Some(modal) = self.closeup_modal.as_mut() {
                        modal.set_error(message);
                    }
                }
                effects
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
        let (available_models, default_model) =
            (self.state.available_models(), self.state.default_model());
        if self.state.overlay() == Some(Overlay::Overview) {
            self.overview_modal.get_or_insert_with(|| {
                OverviewModal::with_selection_mode(self.modal_selection_mode)
            });
        } else {
            self.overview_modal = None;
        }
        if self.state.overlay() == Some(Overlay::Closeup) {
            self.closeup_modal.get_or_insert_with(|| {
                CloseupModal::with_selection_mode(String::new(), self.modal_selection_mode)
                    .with_reopen_choices(self.reopen_choices.clone())
                    .with_agent_models(available_models, default_model)
            });
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
        (matches!(self.state.route(), Route::Home(HomeMode::Closeup))
            || self.state.workspace_agent_drawer_open())
            && self.state.has_live_pane()
            && self.state.overlay().is_none()
            && matches!(self.panes.input_owner(), PaneInputOwner::Tab)
    }

    /// Whether shell-level right-pane controls may mutate their pane state.
    ///
    /// Closeup keeps pending and ready tabs controllable before they own PTY
    /// input. Switch and foreground overlays leave the pane visible but inert.
    #[must_use]
    pub fn wants_pane_control_input(&self) -> bool {
        (matches!(self.state.route(), Route::Home(HomeMode::Closeup))
            || self.state.workspace_agent_drawer_open())
            && self.state.overlay().is_none()
    }

    /// The terminal the active pane's selected tab attaches to, if the selection
    /// is a live tab. The shell polls this terminal for the viewport and forwards
    /// passthrough bytes to it.
    #[must_use]
    pub fn focused_terminal(&self) -> Option<TerminalRef> {
        match self.panes.active_pane().selected() {
            PaneSelection::Tab(TabSelection::Live(terminal)) => Some(terminal.clone()),
            PaneSelection::Tab(
                TabSelection::Pending(_) | TabSelection::Ready(_) | TabSelection::Interrupted(_),
            )
            | PaneSelection::Target(_)
            | PaneSelection::None => None,
        }
    }

    /// The live tabs of every target that are **not** the attached foreground
    /// selection. They own no subscription, so their process exiting can only be
    /// observed through the daemon's per-scope terminal inventory.
    #[must_use]
    pub fn background_terminals(&self) -> Vec<TerminalRef> {
        let focused = self.focused_terminal();
        self.panes
            .live_terminals()
            .into_iter()
            .filter(|terminal| {
                focused
                    .as_ref()
                    .is_none_or(|foreground| !foreground.fences(terminal))
            })
            .collect()
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
        if matches!(target, Target::Root(_)) && kind != PaneKind::Agent {
            return Vec::new();
        }
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

    /// Whether the completion for `operation` is still allowed to select its
    /// pane. Callers use this preview when durable display intent must commit
    /// before the pending tab is promoted into visible runtime state.
    #[must_use]
    pub fn pane_completion_will_focus(&self, operation: OperationId) -> bool {
        self.pane_focus_at_request.get(&operation).copied() == Some(self.state.interaction_count())
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

    /// Close the focused pane tab (Ctrl-O x / Ctrl-O Ctrl-X). Returns the daemon transport work
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
            // An interrupted tab owns no daemon transport: closing it is purely
            // a display dismissal, which the shell persists through #506 intent.
            PaneSelection::Tab(TabSelection::Interrupted(_))
            | PaneSelection::Target(_)
            | PaneSelection::None => CloseOutcome::default(),
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

    /// The interrupted conversation the active pane's selected tab shows, if the
    /// selection is an interrupted tab. The shell uses it to persist the
    /// continuation-scoped dismissal of a closed history tab (#506) and to label
    /// the explicit Resume action.
    #[must_use]
    pub fn focused_interrupted(&self) -> Option<&InterruptedTab> {
        let PaneSelection::Tab(TabSelection::Interrupted(selected)) =
            self.panes.active_pane().selected()
        else {
            return None;
        };
        self.panes
            .active_pane()
            .tabs()
            .iter()
            .find_map(|tab| match tab {
                PaneTab::Interrupted(pane) if pane.tab.continuation == *selected => Some(&pane.tab),
                PaneTab::Interrupted(_)
                | PaneTab::Pending(_)
                | PaneTab::Live(_)
                | PaneTab::Ready(_) => None,
            })
    }

    /// Start the explicit resume of the selected interrupted tab under a fresh
    /// durable `operation`.
    ///
    /// Nothing else can produce this request: inventory refresh, reconnect, and
    /// workspace open never call it. On success exactly that tab becomes pending
    /// and the returned command is the daemon request the shell must send.
    ///
    /// # Errors
    ///
    /// Returns the safe [`ResumeRejection`] when no interrupted tab is selected,
    /// the tab has no trustworthy exact target, or a resume for it is already in
    /// flight. The rejection is also surfaced as the pane's feedback.
    pub fn resume_selected_tab(
        &mut self,
        operation: OperationId,
    ) -> Result<ResumeCommand, ResumeRejection> {
        let Some(target) = self.panes.active() else {
            return Err(ResumeRejection::NotResumable);
        };
        let Some(pane) = self.selected_interrupted_pane() else {
            return Err(ResumeRejection::NotResumable);
        };
        let continuation = pane.tab.continuation;
        let command = resume_command(&pane.tab, pane.resuming, operation);
        match command {
            Ok(command) => {
                let _ = reduce_registry(
                    &mut self.panes,
                    PaneRegistryEvent::Pane {
                        target,
                        event: PaneEvent::ResumeStarted {
                            continuation,
                            operation,
                        },
                    },
                );
                Ok(command)
            }
            Err(rejection) => {
                // A duplicate activation must keep the operation it converged
                // to, so nothing is cleared here.
                self.fail_tab_resume(continuation, None, rejection.safe_message().to_owned());
                Err(rejection)
            }
        }
    }

    /// Turn one resuming interrupted tab into its new live Agent terminal.
    ///
    /// The answer is accepted only when the operation, the lineage, the exact
    /// interrupted source, and a genuinely new fenced terminal all agree
    /// ([`accept_replacement`]). Any disagreement leaves every tab as it was and
    /// surfaces safe feedback instead.
    ///
    /// # Errors
    ///
    /// Returns the safe [`ResumeRejection`] describing which fence disagreed.
    pub fn complete_tab_resume(
        &mut self,
        continuation: AgentContinuationRef,
        answered: OperationId,
        answered_continuation: Option<AgentContinuationRef>,
        relation: Option<&AgentResumeRelation>,
        terminal: &TerminalRef,
    ) -> Result<Vec<PaneRegistryEffect>, ResumeRejection> {
        let Some(target) = self.panes.active() else {
            return Err(ResumeRejection::NotResumable);
        };
        self.complete_tab_resume_for(
            target,
            continuation,
            answered,
            answered_continuation,
            relation,
            terminal,
        )
    }

    /// Complete an exact resume in its owning target even if the drawer was
    /// closed while the daemon request was in flight. A background replacement
    /// updates only that target and produces no attach effect.
    ///
    /// # Errors
    ///
    /// Returns the same exact-resume fence rejection as
    /// [`Self::complete_tab_resume`].
    pub fn complete_tab_resume_for(
        &mut self,
        target: Target,
        continuation: AgentContinuationRef,
        answered: OperationId,
        answered_continuation: Option<AgentContinuationRef>,
        relation: Option<&AgentResumeRelation>,
        terminal: &TerminalRef,
    ) -> Result<Vec<PaneRegistryEffect>, ResumeRejection> {
        if let Err(rejection) = self.validate_tab_resume_for(
            target,
            continuation,
            answered,
            answered_continuation,
            relation,
            terminal,
        ) {
            let operation = self
                .interrupted_pane_for(target, continuation)
                .and_then(|pane| pane.resuming);
            self.fail_tab_resume_for(
                target,
                continuation,
                operation,
                rejection.safe_message().to_owned(),
            );
            return Err(rejection);
        }
        let Some(pane) = self.interrupted_pane_for(target, continuation) else {
            return Err(ResumeRejection::NotResumable);
        };
        let Some(in_flight) = pane.resuming else {
            return Err(ResumeRejection::OperationMismatch);
        };
        let accepted = accept_replacement(
            &pane.tab,
            in_flight,
            answered,
            answered_continuation,
            relation,
            terminal,
        );
        match accepted {
            Ok(replacement) => {
                let effects = reduce_registry(
                    &mut self.panes,
                    PaneRegistryEvent::Pane {
                        target,
                        event: PaneEvent::ResumeReplaced {
                            continuation: replacement.continuation,
                            terminal: replacement.terminal,
                        },
                    },
                );
                self.sync_live_pane();
                Ok(effects)
            }
            Err(rejection) => {
                self.fail_tab_resume_for(
                    target,
                    continuation,
                    Some(in_flight),
                    rejection.safe_message().to_owned(),
                );
                Err(rejection)
            }
        }
    }

    /// Validate an exact resume answer without changing the visible pane.
    ///
    /// # Errors
    ///
    /// Returns the exact operation/source/relation/lineage/scope fence that
    /// rejected the answer.
    pub fn validate_tab_resume_for(
        &self,
        target: Target,
        continuation: AgentContinuationRef,
        answered: OperationId,
        answered_continuation: Option<AgentContinuationRef>,
        relation: Option<&AgentResumeRelation>,
        terminal: &TerminalRef,
    ) -> Result<(), ResumeRejection> {
        let Some(pane) = self.interrupted_pane_for(target, continuation) else {
            return Err(ResumeRejection::NotResumable);
        };
        let Some(in_flight) = pane.resuming else {
            return Err(ResumeRejection::OperationMismatch);
        };
        accept_replacement(
            &pane.tab,
            in_flight,
            answered,
            answered_continuation,
            relation,
            terminal,
        )
        .map(|_| ())
    }

    /// Leave one interrupted tab in place after a refused or failed resume and
    /// show `message`.
    ///
    /// `operation` releases the tab for an explicit retry only when it is the
    /// operation actually in flight; `None` reports feedback without releasing
    /// anything.
    pub fn fail_tab_resume(
        &mut self,
        continuation: AgentContinuationRef,
        operation: Option<OperationId>,
        message: String,
    ) {
        let Some(target) = self.panes.active() else {
            return;
        };
        self.fail_tab_resume_for(target, continuation, operation, message);
    }

    pub fn fail_tab_resume_for(
        &mut self,
        target: Target,
        continuation: AgentContinuationRef,
        operation: Option<OperationId>,
        message: String,
    ) {
        let _ = reduce_registry(
            &mut self.panes,
            PaneRegistryEvent::Pane {
                target,
                event: PaneEvent::ResumeFailed {
                    continuation,
                    operation,
                    message,
                },
            },
        );
    }

    fn selected_interrupted_pane(&self) -> Option<&InterruptedPane> {
        let PaneSelection::Tab(TabSelection::Interrupted(selected)) =
            self.panes.active_pane().selected()
        else {
            return None;
        };
        self.interrupted_pane(*selected)
    }

    fn interrupted_pane(&self, continuation: AgentContinuationRef) -> Option<&InterruptedPane> {
        let target = self.panes.active()?;
        self.interrupted_pane_for(target, continuation)
    }

    fn interrupted_pane_for(
        &self,
        target: Target,
        continuation: AgentContinuationRef,
    ) -> Option<&InterruptedPane> {
        self.panes
            .pane(target)?
            .tabs()
            .iter()
            .find_map(|tab| match tab {
                PaneTab::Interrupted(pane) if pane.tab.continuation == continuation => Some(pane),
                PaneTab::Interrupted(_)
                | PaneTab::Pending(_)
                | PaneTab::Live(_)
                | PaneTab::Ready(_) => None,
            })
    }

    /// Preview the live terminal selected after closing the current live tab.
    /// `Some(None)` means the successor is generic-unaddressable here
    /// (pending/ready) or the target becomes empty; outer `None` means no live
    /// tab is currently selected.
    #[must_use]
    pub fn terminal_after_close(&self) -> Option<Option<TerminalRef>> {
        let PaneSelection::Tab(TabSelection::Live(selected)) = self.panes.active_pane().selected()
        else {
            return None;
        };
        let tabs = self.panes.active_pane().tabs();
        let index = tabs
            .iter()
            .position(|tab| matches!(tab, PaneTab::Live(live) if live.terminal.fences(selected)))?;
        if tabs.len() == 1 {
            return Some(None);
        }
        let successor = if index + 1 < tabs.len() {
            &tabs[index + 1]
        } else {
            &tabs[index - 1]
        };
        Some(match successor {
            PaneTab::Live(live) => Some(live.terminal.clone()),
            PaneTab::Pending(_) | PaneTab::Ready(_) | PaneTab::Interrupted(_) => None,
        })
    }

    /// Preview the stable selection chosen after closing any selected tab.
    #[must_use]
    pub fn selection_after_close(&self) -> Option<Option<TabSelection>> {
        let PaneSelection::Tab(selected) = self.panes.active_pane().selected() else {
            return None;
        };
        let tabs = self.panes.active_pane().tabs();
        let index = tabs
            .iter()
            .position(|tab| tab_selection(tab) == *selected)?;
        if tabs.len() == 1 {
            return Some(None);
        }
        let successor = if index + 1 < tabs.len() {
            &tabs[index + 1]
        } else {
            &tabs[index - 1]
        };
        Some(Some(tab_selection(successor)))
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

    /// Preview the stable live-terminal selection produced by a tab cycle
    /// without mutating the pane registry. The outer `None` means there is no
    /// tab to select; the inner `None` is a pending/ready non-terminal tab.
    #[must_use]
    pub fn terminal_after_select(&self, direction: TabDirection) -> Option<Option<TerminalRef>> {
        self.adjacent_tab(direction)
            .map(|selection| match selection {
                TabSelection::Live(terminal) => Some(terminal),
                TabSelection::Pending(_)
                | TabSelection::Ready(_)
                | TabSelection::Interrupted(_) => None,
            })
    }

    /// Preview the stable selection produced by a tab cycle.
    #[must_use]
    pub fn selection_after_select(&self, direction: TabDirection) -> Option<TabSelection> {
        self.adjacent_tab(direction)
    }

    /// Move the selected tab in the active target while retaining its stable
    /// selection identity. The shell persists the resulting Agent order.
    pub fn reorder_tab(&mut self, direction: TabDirection) -> Vec<PaneRegistryEffect> {
        let effects = route_tab_command(&mut self.panes, PaneTabCommand::Reorder(direction));
        self.sync_live_pane();
        effects
    }

    /// Preview the active pane's live-terminal order after a move so durable
    /// Agent intent can commit before the visible registry changes.
    #[must_use]
    pub fn terminal_order_after_reorder(&self, direction: TabDirection) -> Vec<TerminalRef> {
        let mut panes = self.panes.clone();
        let _ = route_tab_command(&mut panes, PaneTabCommand::Reorder(direction));
        panes
            .active_pane()
            .tabs()
            .iter()
            .filter_map(|tab| match tab {
                PaneTab::Live(pane) => Some(pane.terminal.clone()),
                PaneTab::Pending(_) | PaneTab::Ready(_) | PaneTab::Interrupted(_) => None,
            })
            .collect()
    }

    /// Preview all stable tab identities after a move, including interrupted
    /// lineages used by the root Agent drawer.
    #[must_use]
    pub fn tab_order_after_reorder(&self, direction: TabDirection) -> Vec<TabSelection> {
        let mut panes = self.panes.clone();
        let _ = route_tab_command(&mut panes, PaneTabCommand::Reorder(direction));
        panes
            .active_pane()
            .tabs()
            .iter()
            .map(tab_selection)
            .collect()
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
            PaneSelection::Target(_) | PaneSelection::None => None,
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
        let active = if self.state.workspace_agent_drawer_open() {
            Some(Target::Root(self.state.workspace()))
        } else {
            self.state.active().map(Target::Session)
        };
        if self.panes.active() != active {
            let event = active.map_or(
                PaneRegistryEvent::ClearTarget,
                PaneRegistryEvent::SelectTarget,
            );
            let _ = reduce_registry(&mut self.panes, event);
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
        // A target that owns any tab shows its tab strip, so the action
        // launcher steps aside and a non-live tab (an interrupted Agent history)
        // can be selected and resumed. This is sampled *before* the live level so
        // that losing the last live pane can consult a current tab level and keep
        // a surviving history tab's strip in front.
        let _ = update(
            &mut self.state,
            AppEvent::PaneTabAvailability(self.panes.active_pane().has_tabs()),
        );
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
        let root_cwd = root_cwd.into();
        let projection =
            HomeProjection::from_state(&self.state, workspace_name, &root_cwd, sessions)
                .with_pane(self.panes.active_pane())
                .with_metrics(metrics)
                .with_git_diffs(git_diffs)
                .with_terminal_view(terminal_view)
                .with_workspace_agent_drawer(self.workspace_agent_projection.clone())
                .with_overlay_modals(self.overview_modal.clone(), self.closeup_modal.clone());
        render_home(height, width, &projection)
    }
}

fn tab_selection(tab: &PaneTab) -> TabSelection {
    match tab {
        PaneTab::Pending(pending) => TabSelection::Pending(pending.operation),
        PaneTab::Live(live) => TabSelection::Live(live.terminal.clone()),
        PaneTab::Ready(pending) => TabSelection::Ready(pending.operation),
        PaneTab::Interrupted(pane) => TabSelection::Interrupted(pane.tab.continuation),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentResumeRelation, CloseOutcome, PaneEvent, PaneKind, PaneRegistryEffect,
        PaneRestoreTarget, PaneTab, ResumeRejection, TabSelection, WorkspaceRuntime, tab_selection,
    };
    use crate::usecase::application::Key;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, Effect, HomeMode, Overlay, Route, Selection, TabDirection, Target,
    };
    use crate::usecase::application::pane::{
        LivePane, PaneEffect, PaneRegistry, PaneRegistryEvent, PaneSelection, PendingPane,
        reduce_registry,
    };
    use crate::usecase::terminal_input::LiveTerminalAction;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use usagi_core::domain::id::{
        AgentContinuationRef, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
        WorkspaceId, WorktreeId,
    };
    use usagi_core::domain::settings::{AvailableModels, DefaultModel, ModalSelectionMode};

    #[test]
    fn effective_prompt_mode_is_used_for_both_workspace_modals() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::with_selection_mode(
            workspace,
            vec![session],
            ModalSelectionMode::Prompt,
        );

        let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenOverview));
        assert_eq!(
            runtime.overview_modal().unwrap().selection_mode(),
            ModalSelectionMode::Prompt
        );
        let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));
        let _ = runtime.apply_event(AppEvent::Key(AppKey::OpenCloseupOverlay));
        assert_eq!(
            runtime.closeup_modal().unwrap().selection_mode(),
            ModalSelectionMode::Prompt
        );
        runtime.set_reopen_choices(vec![super::AgentReopenChoice {
            label: "Agent safe".to_owned(),
            continuation: usagi_core::domain::id::AgentContinuationRef::new(),
        }]);
        assert!(runtime.closeup_modal().is_some());
    }

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
        // A single session is selected and active from the start; Enter activates
        // it into Closeup.
        let _ = runtime.handle_key(Key::Enter); // activate → Closeup
        assert_eq!(runtime.state().active(), Some(session));
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
    fn overview_palette_selects_and_replaces_with_emacs_and_shift_keys() {
        let workspace = WorkspaceId::new();
        let mut runtime = overview_on(workspace);
        type_str(&mut runtime, "issue");
        // Ctrl-A/Ctrl-E move the caret to the line edges without opening a session.
        let _ = runtime.handle_key(Key::LineStart);
        assert_eq!(runtime.overview_modal().unwrap().cursor(), 0);
        let _ = runtime.handle_key(Key::LineEnd);
        assert_eq!(runtime.overview_modal().unwrap().cursor(), 5);
        // Home/End behave the same inside the focused palette.
        let _ = runtime.handle_key(Key::Home);
        assert_eq!(runtime.overview_modal().unwrap().cursor(), 0);
        let _ = runtime.handle_key(Key::End);
        // Shift+Home selects the whole line; Delete drops the selection.
        let _ = runtime.handle_key(Key::SelectHome);
        assert_eq!(runtime.overview_modal().unwrap().selection(), Some((0, 5)));
        let _ = runtime.handle_key(Key::Delete);
        assert_eq!(runtime.overview_modal().unwrap().input(), "");
        // Shift+arrow / Shift+End extend a fresh selection.
        type_str(&mut runtime, "abc");
        let _ = runtime.handle_key(Key::Home);
        let _ = runtime.handle_key(Key::SelectRight);
        assert_eq!(runtime.overview_modal().unwrap().selection(), Some((0, 1)));
        let _ = runtime.handle_key(Key::SelectEnd);
        assert_eq!(runtime.overview_modal().unwrap().selection(), Some((0, 3)));
        let _ = runtime.handle_key(Key::SelectLeft);
        assert_eq!(runtime.overview_modal().unwrap().selection(), Some((0, 2)));
        // The sidebar never moved while the palette owned every key.
        assert_eq!(runtime.state().overlay(), Some(Overlay::Overview));
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
    fn a_refused_closeup_submission_shows_its_reason_in_the_still_open_modal() {
        // A refusal produces no effect and leaves the overlay open. Without the
        // reducer's message reaching the modal, Enter looked like a dead key and
        // the user could only conclude that the Agent would not start.
        let mut runtime = closeup_on(WorkspaceId::new(), SessionId::new());
        runtime.set_agent_models(AvailableModels::default(), DefaultModel::OpenAi);
        assert!(runtime.closeup_modal().unwrap().error().is_none());

        let effects = runtime.handle_key(Key::Enter);
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
        assert_eq!(
            runtime.closeup_modal().unwrap().error(),
            Some("the configured agent CLI is not installed")
        );

        // Editing the input clears the stale reason.
        let _ = runtime.handle_key(Key::Char('c'));
        assert!(runtime.closeup_modal().unwrap().error().is_none());

        // An accepted submission closes the overlay, so no reason can linger.
        runtime.set_agent_models(AvailableModels::all(), DefaultModel::OpenAi);
        let _ = runtime.handle_key(Key::Backspace);
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::LaunchAgent { .. })),
            "{effects:?}"
        );
        assert!(runtime.closeup_modal().is_none());
    }

    #[test]
    fn agent_model_policy_reaches_the_reducer_and_an_open_closeup_modal() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);

        // A policy applied while Closeup is open re-projects the live modal, so
        // its `-m` picker follows a newly saved setting immediately.
        runtime.set_agent_models(
            AvailableModels::new([DefaultModel::SakanaAi]),
            DefaultModel::SakanaAi,
        );
        assert_eq!(
            runtime.state().available_models(),
            AvailableModels::new([DefaultModel::SakanaAi])
        );
        assert_eq!(runtime.state().default_model(), DefaultModel::SakanaAi);
        let modal = runtime.closeup_modal().unwrap().clone();
        assert_eq!(
            modal.with_agent_models(
                AvailableModels::new([DefaultModel::SakanaAi]),
                DefaultModel::SakanaAi
            ),
            *runtime.closeup_modal().unwrap()
        );

        // Right expands the `agent` row into the single installed choice and
        // Enter launches that profile.
        let _ = runtime.handle_key(Key::Right);
        assert_eq!(
            runtime.closeup_modal().unwrap().submission(),
            "agent -m sakana.ai"
        );
        let effects = runtime.handle_key(Key::Enter);
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::LaunchAgent { profile: Some(profile), .. }
                    if profile.as_str() == "sakana-ai"
            )),
            "{effects:?}"
        );
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
    fn closeup_prompt_caret_uses_home_end_and_shift_selection() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        // Left/Right drive the action picker here, so caret motion is Home/End
        // (and emacs Ctrl-A/Ctrl-E); selection extends with Shift+arrow.
        type_str(&mut runtime, "close");
        let _ = runtime.handle_key(Key::LineStart);
        assert_eq!(runtime.closeup_modal().unwrap().selection(), None);
        let _ = runtime.handle_key(Key::SelectEnd);
        assert_eq!(runtime.closeup_modal().unwrap().selection(), Some((0, 5)));
        let _ = runtime.handle_key(Key::Delete);
        // Home / SelectRight / SelectLeft / LineEnd all reach the prompt input.
        type_str(&mut runtime, "abc");
        let _ = runtime.handle_key(Key::Home);
        let _ = runtime.handle_key(Key::SelectRight);
        assert_eq!(runtime.closeup_modal().unwrap().selection(), Some((0, 1)));
        let _ = runtime.handle_key(Key::End);
        let _ = runtime.handle_key(Key::SelectLeft);
        assert_eq!(runtime.closeup_modal().unwrap().selection(), Some((2, 3)));
        let _ = runtime.handle_key(Key::SelectHome);
        let _ = runtime.handle_key(Key::LineEnd);
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
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
        assert!(runtime.wants_pane_control_input());
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::OpenCloseupModal));
        assert_eq!(runtime.state().overlay(), Some(Overlay::Closeup));
        assert!(runtime.closeup_modal().is_some());
        assert!(!runtime.wants_live_input());
        assert!(!runtime.wants_pane_control_input());
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
        assert!(!runtime.wants_pane_control_input());
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
    fn new_starts_on_the_first_session_with_an_empty_pane() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let runtime = WorkspaceRuntime::new(workspace, vec![session]);
        assert_eq!(runtime.state().active(), Some(session));
        assert_eq!(runtime.panes().active(), Some(Target::Session(session)));
        assert!(runtime.active_pane().tabs().is_empty());
        assert!(!runtime.state().has_live_pane());
        assert!(!runtime.wants_live_input());
    }

    #[test]
    fn empty_home_has_no_active_pane_target() {
        let workspace = WorkspaceId::new();
        let runtime = WorkspaceRuntime::new(workspace, Vec::new());
        assert_eq!(runtime.state().active(), None);
        assert_eq!(runtime.panes().active(), None);
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
        // The single session is already selected; Enter activates it.
        let effects = runtime.handle_key(Key::Enter);
        assert!(effects.is_empty());
        assert_eq!(runtime.state().active(), Some(session));
        // Passthrough never reaches the reducer.
        assert!(runtime.handle_key(Key::Passthrough(vec![0x1b])).is_empty());
    }

    #[test]
    fn new_session_row_enter_emits_create_effect() {
        let workspace = WorkspaceId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());
        // An empty Home already rests on `+ new session`; open the form.
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
        assert_eq!(runtime.panes().active(), Some(Target::Session(session)));
    }

    #[test]
    fn double_click_switches_to_an_append_restored_session_with_live_input_focus() {
        let workspace = WorkspaceId::new();
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let first_terminal = terminal_ref(workspace, first_session);
        let second_terminal = terminal_ref(workspace, second_session);
        let mut runtime = WorkspaceRuntime::new(workspace, vec![first_session, second_session]);
        let (interaction, revision) = runtime.restore_fence();

        assert!(runtime.append_restore_snapshot(
            interaction,
            revision,
            vec![
                PaneRestoreTarget {
                    target: Target::Session(first_session),
                    panes: vec![LivePane {
                        terminal: first_terminal,
                        kind: PaneKind::Terminal,
                    }],
                    selected: None,
                    selected_interrupted: None,
                    interrupted: Vec::new(),
                },
                PaneRestoreTarget {
                    target: Target::Session(second_session),
                    panes: vec![LivePane {
                        terminal: second_terminal.clone(),
                        kind: PaneKind::Agent,
                    }],
                    selected: None,
                    selected_interrupted: None,
                    interrupted: Vec::new(),
                },
            ],
        ));
        let _ = runtime.apply_event(AppEvent::Resize {
            width: 100,
            height: 30,
        });
        for at in [1_000, 1_100] {
            let _ = runtime.apply_event(AppEvent::Pointer {
                column: 5,
                row: 4,
                at: std::time::Duration::from_millis(at),
            });
        }

        assert_eq!(runtime.state().active(), Some(second_session));
        assert_eq!(
            runtime.panes().active(),
            Some(Target::Session(second_session))
        );
        assert_eq!(runtime.focused_terminal(), Some(second_terminal));
        assert!(runtime.wants_live_input());
        assert_eq!(runtime.state().overlay(), None);
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
    fn tab_previews_cover_empty_pending_and_previous_successors() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        assert_eq!(runtime.terminal_after_close(), None);

        let first_operation = OperationId::new();
        let first = terminal_ref(workspace, session);
        let _ = runtime.request_pane(target, first_operation, PaneKind::Agent);
        let _ = runtime.complete_pane(target, first_operation, first.clone());
        let pending = OperationId::new();
        let _ = runtime.request_pane(target, pending, PaneKind::Agent);
        let _ = runtime.focus_terminal(target, first.clone());

        assert_eq!(runtime.terminal_after_close(), Some(None));
        assert_eq!(
            runtime.terminal_after_select(TabDirection::Next),
            Some(None)
        );
        assert_eq!(
            runtime.terminal_order_after_reorder(TabDirection::Next),
            vec![first.clone()]
        );

        let second = terminal_ref(workspace, session);
        let _ = runtime.complete_pane(target, pending, second.clone());
        let _ = runtime.focus_terminal(target, second);
        assert_eq!(runtime.terminal_after_close(), Some(Some(first)));
    }

    #[test]
    fn reorder_tab_moves_the_selected_stable_identity_without_refocusing() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let mut runtime = closeup_on(workspace, session);
        let first = terminal_ref(workspace, session);
        let second = terminal_ref(workspace, session);
        for terminal in [first.clone(), second.clone()] {
            let operation = OperationId::new();
            let _ = runtime.request_pane(target, operation, PaneKind::Agent);
            let _ = runtime.complete_pane(target, operation, terminal);
        }
        let _ = runtime.focus_terminal(target, first.clone());

        let _ = runtime.reorder_tab(TabDirection::Next);
        assert_eq!(runtime.focused_terminal(), Some(first.clone()));
        assert!(matches!(
            runtime.active_pane().tabs(),
            [PaneTab::Live(left), PaneTab::Live(right)]
                if left.terminal == second && right.terminal == first
        ));
        let _ = runtime.reorder_tab(TabDirection::Previous);
        assert!(matches!(
            runtime.active_pane().tabs(),
            [PaneTab::Live(left), PaneTab::Live(right)]
                if left.terminal == first && right.terminal == second
        ));
    }

    #[test]
    fn delayed_restore_changes_nothing_after_newer_interaction() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let target = Target::Session(session);
        let first = terminal_ref(workspace, session);
        let second = terminal_ref(workspace, session);
        let discovered = terminal_ref(workspace, session);
        let mut runtime = closeup_on(workspace, session);
        let (dispatched_interaction, dispatched_revision) = runtime.restore_fence();
        for terminal in [first.clone(), second.clone()] {
            let operation = OperationId::new();
            let _ = runtime.request_pane(target, operation, PaneKind::Agent);
            let _ = runtime.complete_pane(target, operation, terminal);
        }
        let _ = runtime.focus_terminal(target, first.clone());
        let _ = runtime.reorder_tab(TabDirection::Next);
        let newer_order = runtime.active_pane().tabs().to_vec();

        let accepted = runtime.restore_snapshot(
            dispatched_interaction,
            dispatched_revision,
            vec![PaneRestoreTarget {
                target,
                panes: vec![
                    LivePane {
                        terminal: first.clone(),
                        kind: PaneKind::Agent,
                    },
                    LivePane {
                        terminal: discovered.clone(),
                        kind: PaneKind::Agent,
                    },
                ],
                selected: Some(discovered),
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        );

        assert!(!accepted);
        assert_eq!(runtime.focused_terminal(), Some(first));
        assert_eq!(runtime.active_pane().tabs(), newer_order.as_slice());
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
    fn on_effect_records_a_terminal_placeholder_for_the_active_session() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        runtime.on_effect(&Effect::OpenTerminal {
            target: Target::Session(session),
            operation_id: OperationId::new(),
            arguments: String::new(),
        });
        // The active session owns a pane strip: the request records a pending
        // placeholder the daemon completion later promotes.
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
    fn workspace_agent_live_action_toggles_from_switch_and_closeup_without_pane_drift() {
        let workspace = WorkspaceId::new();
        let first = SessionId::new();
        let second = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![first, second]);

        let switch_background = (
            runtime.state().route(),
            runtime.state().selected(),
            runtime.state().active(),
            runtime.active_pane().clone(),
        );
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::WorkspaceAgent));
        assert!(runtime.state().workspace_agent_drawer_open());
        let _ = runtime.handle_key(Key::Down);
        let _ = runtime.handle_key(Key::Enter);
        assert_eq!(
            (
                runtime.state().route(),
                runtime.state().selected(),
                runtime.state().active(),
                runtime.panes().pane(Target::Session(first)).cloned(),
            ),
            (
                switch_background.0,
                switch_background.1,
                switch_background.2,
                Some(switch_background.3),
            )
        );
        assert_eq!(runtime.panes().active(), Some(Target::Root(workspace)));
        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::WorkspaceAgent));
        assert!(!runtime.state().workspace_agent_drawer_open());

        let _ = runtime.handle_key(Key::Down);
        let _ = runtime.handle_key(Key::Enter);
        let target = Target::Session(second);
        let first_operation = OperationId::new();
        let second_operation = OperationId::new();
        let _ = runtime.request_pane(target, first_operation, PaneKind::Terminal);
        let _ = runtime.request_pane(target, second_operation, PaneKind::Agent);
        let _ = runtime.select_tab(TabDirection::Previous);
        let closeup_background = (
            runtime.state().route(),
            runtime.state().selected(),
            runtime.state().active(),
            runtime.active_pane().clone(),
        );

        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::WorkspaceAgent));
        assert!(runtime.state().workspace_agent_drawer_open());
        assert!(!runtime.wants_live_input());
        assert!(!runtime.wants_pane_control_input());
        for key in [
            Key::Live(LiveTerminalAction::NextTab),
            Key::Live(LiveTerminalAction::CloseTab),
            Key::Up,
            Key::CtrlQ,
            Key::Char(':'),
        ] {
            assert!(runtime.handle_key(key).is_empty());
        }
        assert_eq!(
            (
                runtime.state().route(),
                runtime.state().selected(),
                runtime.state().active(),
                runtime.panes().pane(target).cloned(),
            ),
            (
                closeup_background.0,
                closeup_background.1,
                closeup_background.2,
                Some(closeup_background.3.clone()),
            )
        );
        let _ = runtime.handle_key(Key::Escape);
        assert!(runtime.state().workspace_agent_drawer_open());
        let _ = runtime.handle_key(Key::Escape);
        assert!(!runtime.state().workspace_agent_drawer_open());
        assert_eq!(runtime.active_pane(), &closeup_background.3);
    }

    #[test]
    fn workspace_agent_drawer_hands_foreground_to_agent_only_root_and_back() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let managed = terminal_ref(workspace, session);
        let root_agent = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        let root_generic = TerminalRef {
            terminal_id: TerminalId::new(),
            ..root_agent.clone()
        };
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![
                PaneRestoreTarget {
                    target: Target::Session(session),
                    panes: vec![LivePane {
                        terminal: managed.clone(),
                        kind: PaneKind::Terminal,
                    }],
                    selected: Some(managed.clone()),
                    selected_interrupted: None,
                    interrupted: Vec::new(),
                },
                PaneRestoreTarget {
                    target: Target::Root(workspace),
                    panes: vec![
                        LivePane {
                            terminal: root_agent.clone(),
                            kind: PaneKind::Agent,
                        },
                        LivePane {
                            terminal: root_generic,
                            kind: PaneKind::Terminal,
                        },
                    ],
                    selected: Some(root_agent.clone()),
                    selected_interrupted: None,
                    interrupted: Vec::new(),
                },
            ],
        ));
        assert_eq!(runtime.focused_terminal(), Some(managed.clone()));

        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::WorkspaceAgent));
        assert_eq!(runtime.panes().active(), Some(Target::Root(workspace)));
        assert_eq!(runtime.focused_terminal(), Some(root_agent));
        assert!(runtime.wants_live_input());
        assert!(
            runtime
                .active_pane()
                .tabs()
                .iter()
                .all(|tab| matches!(tab, PaneTab::Live(live) if live.kind == PaneKind::Agent))
        );

        let _ = runtime.handle_key(Key::Escape);
        assert_eq!(runtime.panes().active(), Some(Target::Session(session)));
        assert_eq!(runtime.focused_terminal(), Some(managed));
    }

    #[test]
    fn workspace_agent_restore_selects_interrupted_root_without_resuming() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut history = interrupted_tab(workspace, session, true);
        history.session_id = None;
        history.last_terminal.session_id = None;
        if let Some(target) = history.target.as_mut() {
            target.session_id = None;
        }
        let continuation = history.continuation;
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let fence = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            fence.0,
            fence.1,
            vec![PaneRestoreTarget {
                target: Target::Root(workspace),
                panes: Vec::new(),
                selected: None,
                selected_interrupted: Some(continuation),
                interrupted: vec![history],
            }],
        ));

        let _ = runtime.handle_key(Key::Live(LiveTerminalAction::WorkspaceAgent));
        assert_eq!(
            runtime.active_pane().selected(),
            &PaneSelection::Tab(TabSelection::Interrupted(continuation))
        );
        assert_eq!(
            runtime.focused_interrupted().map(|tab| tab.continuation),
            Some(continuation)
        );
        assert!(runtime.focused_terminal().is_none());
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
            agent_resume: None,
            lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle::Available,
            failure_summary: None,
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

    /// One interrupted lineage of `session` in `workspace`, with a trustworthy
    /// exact target unless `resumable` is false.
    fn interrupted_tab(
        workspace: WorkspaceId,
        session: SessionId,
        resumable: bool,
    ) -> super::InterruptedTab {
        use usagi_core::domain::agent::{
            AgentResumeTarget, ProviderKind, ProviderResumePhase, ProviderResumeReason,
        };
        use usagi_core::domain::id::{AgentResumeSourceId, AgentRuntimeId};

        let terminal = terminal_ref(workspace, session);
        let continuation = usagi_core::domain::id::AgentContinuationRef::new();
        super::InterruptedTab {
            continuation,
            session_id: Some(session),
            provider: Some(ProviderKind::Codex),
            last_known_phase: Some(ProviderResumePhase::Interrupted),
            reason: if resumable {
                ProviderResumeReason::ExplicitResumeAvailable
            } else {
                ProviderResumeReason::ProviderMetadataUnavailable
            },
            target: resumable.then(|| AgentResumeTarget {
                continuation,
                source: AgentResumeSourceId::new(),
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: terminal.worktree_id,
                runtime_id: AgentRuntimeId::new(),
                adapter_revision: 3,
            }),
            last_terminal: terminal,
        }
    }

    /// The replacement one accepted resume of `tab` would produce.
    fn replacement(tab: &super::InterruptedTab) -> (AgentResumeRelation, TerminalRef) {
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: tab.last_terminal.workspace_id,
            session_id: tab.session_id,
            worktree_id: tab.last_terminal.worktree_id,
        };
        (
            AgentResumeRelation {
                source: tab.target.as_ref().unwrap().source,
                replacement_runtime: usagi_core::domain::id::AgentRuntimeId::new(),
                replacement_terminal: terminal.clone(),
            },
            terminal,
        )
    }

    /// Seed one target's interrupted history through the restore fence and select
    /// the first tab, as the shell does after a coherent observation.
    fn with_history(
        runtime: &mut WorkspaceRuntime,
        target: Target,
        tabs: Vec<super::InterruptedTab>,
    ) {
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            interaction,
            revision,
            vec![PaneRestoreTarget {
                target,
                panes: Vec::new(),
                selected: None,
                selected_interrupted: None,
                interrupted: tabs,
            }],
        ));
    }

    /// #544: a target whose only tabs are interrupted history must open Closeup
    /// on its tab strip, not behind the action launcher.
    ///
    /// The restore lands while Home is still in Switch, so activation sees no
    /// availability *edge*. If activation decided the overlay from the live-pane
    /// level alone, the launcher would cover the strip and swallow every
    /// `Ctrl-O` pane control, leaving `Ctrl-O r` unreachable from real keys.
    #[test]
    fn a_history_only_target_activates_onto_its_tab_strip_not_the_action_launcher() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = WorkspaceRuntime::new(workspace, vec![session]);
        let history = interrupted_tab(workspace, session, true);
        // The managed session is initially selected, so restore its history first
        // and activate afterwards: exactly the cold-restart order the shell observes.
        with_history(
            &mut runtime,
            Target::Session(session),
            vec![history.clone()],
        );
        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Switch)
        ));

        let _ = runtime.handle_key(Key::Enter);

        assert!(matches!(
            runtime.state().route(),
            Route::Home(HomeMode::Closeup)
        ));
        assert!(!runtime.state().has_live_pane());
        assert!(
            runtime.wants_pane_control_input(),
            "the launcher must step aside for a target that owns tabs"
        );
        // Tab cycling is a tab-strip concern, not a live-PTY one.
        for effect in runtime.handle_key(Key::Live(
            crate::usecase::terminal_input::LiveTerminalAction::NextTab,
        )) {
            runtime.on_effect(&effect);
        }
        assert_eq!(
            runtime.focused_interrupted().map(|tab| tab.continuation),
            Some(history.continuation)
        );
    }

    /// #544: losing the last *live* pane is not an empty Closeup while an
    /// interrupted history tab survives; the strip keeps its pane controls.
    #[test]
    fn closing_the_last_live_pane_keeps_a_surviving_history_tab_in_front() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        let history = interrupted_tab(workspace, session, true);
        let live = terminal_ref(workspace, session);
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.restore_snapshot(
            interaction,
            revision,
            vec![PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: live.clone(),
                    kind: PaneKind::Agent,
                }],
                selected: Some(live),
                selected_interrupted: None,
                interrupted: vec![history.clone()],
            }],
        ));
        assert!(runtime.state().has_live_pane());
        assert!(runtime.wants_pane_control_input());

        let outcome = runtime.close_focused_pane();
        assert!(outcome.detach.is_some());

        assert!(!runtime.state().has_live_pane());
        assert!(
            runtime.wants_pane_control_input(),
            "the surviving history tab keeps the strip in front of the launcher"
        );
        assert_eq!(
            tab_selection(&runtime.active_pane().tabs()[0]),
            TabSelection::Interrupted(history.continuation)
        );
    }

    #[test]
    fn a_restored_history_tab_owns_no_terminal_and_closes_without_daemon_work() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        let history = interrupted_tab(workspace, session, true);
        with_history(
            &mut runtime,
            Target::Session(session),
            vec![history.clone()],
        );

        assert_eq!(
            tab_selection(&runtime.active_pane().tabs()[0]),
            TabSelection::Interrupted(history.continuation)
        );
        // A history tab is not a live pane: nothing to attach, poll, or resize.
        assert!(!runtime.state().has_live_pane());
        let _ = runtime.select_tab(TabDirection::Next);
        assert_eq!(
            runtime.focused_interrupted().map(|tab| tab.continuation),
            Some(history.continuation)
        );
        assert!(runtime.focused_terminal().is_none());
        assert_eq!(runtime.terminal_after_close(), None);
        assert_eq!(
            runtime.terminal_after_select(TabDirection::Next),
            Some(None)
        );
        assert!(
            runtime
                .terminal_order_after_reorder(TabDirection::Next)
                .is_empty()
        );

        assert_eq!(runtime.close_focused_pane(), CloseOutcome::default());
        assert!(!runtime.active_pane().has_tabs());
    }

    #[test]
    fn an_explicit_resume_pends_one_tab_and_a_validated_replacement_turns_it_live() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        let resumed = interrupted_tab(workspace, session, true);
        let other = interrupted_tab(workspace, session, true);
        with_history(
            &mut runtime,
            Target::Session(session),
            vec![resumed.clone(), other.clone()],
        );
        let _ = runtime.select_tab(TabDirection::Next);

        let command = runtime.resume_selected_tab(OperationId::new()).unwrap();
        assert_eq!(command.target, *resumed.target.as_ref().unwrap());
        // A repeated activation converges to the in-flight operation instead of
        // sending a second request.
        assert_eq!(
            runtime.resume_selected_tab(OperationId::new()),
            Err(ResumeRejection::AlreadyResuming)
        );

        let (relation, terminal) = replacement(&resumed);
        let effects = runtime
            .complete_tab_resume(
                resumed.continuation,
                command.operation,
                Some(resumed.continuation),
                Some(&relation),
                &terminal,
            )
            .unwrap();
        assert!(effects.iter().any(|effect| matches!(
            effect,
            PaneRegistryEffect::Pane { effect: PaneEffect::Attach(attached), .. }
                if *attached == terminal
        )));
        assert_eq!(runtime.focused_terminal(), Some(terminal));
        assert!(runtime.state().has_live_pane());
        // The other history tab is untouched.
        assert_eq!(runtime.active_pane().tabs().len(), 2);
    }

    #[test]
    fn resume_refuses_a_tab_without_a_target_a_missing_selection_and_a_stale_answer() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);

        // No interrupted tab is selected at all.
        assert_eq!(
            runtime.resume_selected_tab(OperationId::new()),
            Err(ResumeRejection::NotResumable)
        );

        let unresumable = interrupted_tab(workspace, session, false);
        let resumable = interrupted_tab(workspace, session, true);
        with_history(
            &mut runtime,
            Target::Session(session),
            vec![unresumable.clone(), resumable.clone()],
        );
        let _ = runtime.select_tab(TabDirection::Next);
        assert_eq!(
            runtime.resume_selected_tab(OperationId::new()),
            Err(ResumeRejection::NotResumable)
        );
        assert_eq!(
            runtime.active_pane().error(),
            Some(ResumeRejection::NotResumable.safe_message())
        );
        // Selecting the second history skips the first tab while resolving the
        // selection, and finds the resumable lineage.
        let _ = runtime.select_tab(TabDirection::Next);
        assert_eq!(
            runtime.focused_interrupted().map(|tab| tab.continuation),
            Some(resumable.continuation)
        );
        let _ = runtime.select_tab(TabDirection::Previous);

        // An answer for a lineage nobody is resuming is refused, and so is one
        // whose operation does not match the in-flight request.
        let (relation, terminal) = replacement(&resumable);
        assert_eq!(
            runtime.complete_tab_resume(
                usagi_core::domain::id::AgentContinuationRef::new(),
                OperationId::new(),
                Some(resumable.continuation),
                Some(&relation),
                &terminal,
            ),
            Err(ResumeRejection::NotResumable)
        );
        assert_eq!(
            runtime.complete_tab_resume(
                resumable.continuation,
                OperationId::new(),
                Some(resumable.continuation),
                Some(&relation),
                &terminal,
            ),
            Err(ResumeRejection::OperationMismatch)
        );

        let _ = runtime.select_tab(TabDirection::Next);
        let command = runtime.resume_selected_tab(OperationId::new()).unwrap();
        // A daemon answer without the source-to-replacement relation leaves the
        // history tab in place and shows safe feedback.
        assert_eq!(
            runtime.complete_tab_resume(
                resumable.continuation,
                command.operation,
                Some(resumable.continuation),
                None,
                &terminal,
            ),
            Err(ResumeRejection::RelationMissing)
        );
        assert_eq!(
            runtime.active_pane().error(),
            Some(ResumeRejection::RelationMissing.safe_message())
        );
        assert_eq!(runtime.active_pane().tabs().len(), 2);
        assert!(!runtime.state().has_live_pane());

        // The refusal cleared the in-flight marker, so an explicit retry works.
        let retry = runtime.resume_selected_tab(OperationId::new()).unwrap();
        assert_ne!(retry.operation, command.operation);
    }

    #[test]
    fn a_second_session_history_tab_resumes_with_the_same_ux() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        assert_eq!(runtime.panes().active(), Some(Target::Session(session)));

        let history = interrupted_tab(workspace, session, true);
        with_history(
            &mut runtime,
            Target::Session(session),
            vec![history.clone()],
        );
        let _ = runtime.select_tab(TabDirection::Next);

        let command = runtime.resume_selected_tab(OperationId::new()).unwrap();
        let (relation, terminal) = replacement(&history);
        assert!(
            runtime
                .complete_tab_resume(
                    history.continuation,
                    command.operation,
                    Some(history.continuation),
                    Some(&relation),
                    &terminal,
                )
                .is_ok()
        );
        assert_eq!(runtime.focused_terminal(), Some(terminal));
    }

    #[test]
    fn fail_tab_resume_reports_a_transport_failure_without_touching_other_tabs() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        let history = interrupted_tab(workspace, session, true);
        with_history(
            &mut runtime,
            Target::Session(session),
            vec![history.clone()],
        );
        let _ = runtime.select_tab(TabDirection::Next);
        let command = runtime.resume_selected_tab(OperationId::new()).unwrap();

        runtime.fail_tab_resume(
            history.continuation,
            Some(command.operation),
            "daemon request failed".to_owned(),
        );
        assert_eq!(runtime.active_pane().error(), Some("daemon request failed"));
        assert_eq!(runtime.active_pane().tabs().len(), 1);
        // Resumable again after the failure.
        assert!(runtime.resume_selected_tab(OperationId::new()).is_ok());
    }

    #[test]
    fn an_append_only_restore_preserves_history_tabs_it_cannot_vouch_for() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut runtime = closeup_on(workspace, session);
        let history = interrupted_tab(workspace, session, true);
        with_history(
            &mut runtime,
            Target::Session(session),
            vec![history.clone()],
        );

        // The append path runs when Agent intent persistence failed, so its
        // observation is not display intent: it adds generic panes without
        // dropping the interrupted history it carries no projection for.
        let generic = terminal_ref(workspace, session);
        let (interaction, revision) = runtime.restore_fence();
        assert!(runtime.append_restore_snapshot(
            interaction,
            revision,
            vec![PaneRestoreTarget {
                target: Target::Session(session),
                panes: vec![LivePane {
                    terminal: generic.clone(),
                    kind: PaneKind::Terminal,
                }],
                selected: None,
                selected_interrupted: None,
                interrupted: Vec::new(),
            }],
        ));

        assert!(
            runtime
                .active_pane()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Interrupted(pane)
                    if pane.tab.continuation == history.continuation))
        );
        assert!(
            runtime
                .active_pane()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Live(live) if live.terminal == generic))
        );
    }

    #[test]
    fn resume_entry_points_are_inert_without_an_active_managed_target() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let mut runtime = WorkspaceRuntime::new(workspace, Vec::new());

        assert_eq!(
            runtime.resume_selected_tab(OperationId::new()),
            Err(ResumeRejection::NotResumable)
        );
        assert_eq!(
            runtime.complete_tab_resume(
                continuation,
                OperationId::new(),
                Some(continuation),
                None,
                &terminal_ref(workspace, session),
            ),
            Err(ResumeRejection::NotResumable)
        );
        runtime.fail_tab_resume(continuation, None, "ignored".to_owned());
        assert_eq!(runtime.panes().active(), None);
    }
}

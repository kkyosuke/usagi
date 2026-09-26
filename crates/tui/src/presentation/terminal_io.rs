//! pane / terminal の起動・入力転送・選択・投影。

#[cfg(test)]
use super::TERMINAL_PROJECTION_BUILDS;
use super::{
    AgentContinuationRef, AgentPaneAdmission, AgentProfileId, AgentResumeTarget,
    AgentTabIntentMutation, AppEvent, AppKey, BrowserOpener, ExactAgentResume,
    ExternalTerminalPort, FRAME_EVENT_BUDGET, Geometry, Key, LiveTerminalAction,
    LiveTerminalControls, Notice, OperationId, PANE_LAUNCH_BUSY, PANE_LAUNCH_FIRST,
    PANE_LAUNCH_QUEUE_LIMIT, PANE_LAUNCH_UNADMITTED, PANE_LAUNCH_WORKER_FAILED,
    PaneLaunchCommandPort, PasteMode, Path, PointerEvent, PointerKind, PointerRelease, SessionId,
    TabSelection, Target, Terminal, TerminalRef, TerminalViewProjection, WHEEL_LINES,
    WorkspaceDrawerFocus, WorkspaceId, WorkspaceIoRuntime, WorkspaceRuntime,
    activate_focused_interrupted_tab, apply_agent_launch_completion, apply_exact_resume,
    dismiss_interrupted_history, encode_mouse_wheel, encode_wheel_arrows, is_director_new_click,
    is_director_new_pointer, select_director_tab_and_activate, surface_agent_tab_intent_error,
    terminal_point_at,
};

/// Keeps an embedder without a daemon launch client safe: every pane launch
/// becomes one inline failure and nothing is spawned locally.
pub(super) struct UnavailablePaneLaunchPort;

impl PaneLaunchCommandPort for UnavailablePaneLaunchPort {
    fn launch(
        &self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent launch is unavailable".to_owned())
    }

    fn resume(
        &self,
        _workspace: WorkspaceId,
        _session: SessionId,
        _operation: OperationId,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent resume is unavailable.".to_owned())
    }

    fn resume_exact(
        &self,
        _target: AgentResumeTarget,
        _operation: OperationId,
    ) -> Result<ExactAgentResume, String> {
        Err("Exact Agent resume is unavailable.".to_owned())
    }

    fn launch_terminal(
        &self,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _geometry: Geometry,
        _arguments: &str,
        _operation: OperationId,
    ) -> Result<TerminalRef, String> {
        Err("terminal launch is unavailable".to_owned())
    }
}

/// Maps a management [`Key`] to the bytes a focused live terminal should
/// receive. Reserved prefix actions ([`Key::Live`]) do not reach the shell;
/// all other keys, including global controls, do while Closeup owns the pane.
#[cfg(test)]
pub(super) fn key_to_terminal_bytes(key: Key) -> Option<Vec<u8>> {
    key_to_terminal_bytes_for_mode(key, false)
}

pub(super) fn key_to_terminal_bytes_for_mode(key: Key, bracketed_paste: bool) -> Option<Vec<u8>> {
    let bytes = match key {
        Key::Passthrough(bytes) => return (!bytes.is_empty()).then(|| bytes.clone()),
        Key::Management { passthrough, .. } => {
            return (!passthrough.is_empty()).then_some(passthrough);
        }
        // Mark a paste only when the focused program requested DECSET 2004.
        // Otherwise those control sequences can become visible `200~` / `201~`
        // text in a shell or an Agent that has not enabled bracketed paste yet.
        Key::Paste(text) => {
            return (!text.is_empty())
                .then(|| crate::usecase::terminal_input::encode_paste(&text, bracketed_paste));
        }
        Key::Char(ch) => ch.to_string().into_bytes(),
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Up => b"\x1b[A".to_vec(),
        Key::Down => b"\x1b[B".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Right | Key::SelectRight => b"\x1b[C".to_vec(),
        Key::Left | Key::SelectLeft => b"\x1b[D".to_vec(),
        // The focused shell owns its own line editing: forward Home/Ctrl-A and
        // End/Ctrl-E as the readline control chords the previous mapping sent, so
        // caret keys that mean selection to a text field keep moving in the shell.
        Key::Home | Key::LineStart | Key::SelectHome => vec![1],
        Key::End | Key::LineEnd | Key::SelectEnd => vec![5],
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Quit => vec![3],
        Key::CtrlQ => vec![17],
        Key::CtrlD => vec![4],
        Key::CtrlX => vec![24],
        // Contextual help is presentation-owned and must never reach a PTY.
        Key::Help => return None,
        Key::Live(_)
        | Key::TerminalCopy { .. }
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Resize
        | Key::Other => {
            return None;
        }
    };
    Some(bytes)
}

/// Forward one ordinary key to the focused Closeup terminal. Returns `true`
/// when the live pane owned the key, including the busy/error case where the
/// keystroke could not be delivered and a safe notice was recorded.
pub(super) fn forward_live_terminal_input(
    ui: &mut WorkspaceIoRuntime,
    runtime: &WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    key: &Key,
) -> bool {
    if let Key::TerminalCopy { fallback } = key {
        let Some(terminal) = runtime
            .wants_live_input()
            .then(|| runtime.focused_terminal())
            .flatten()
        else {
            return false;
        };
        if controls.has_selection() {
            copy_terminal_selection(controls, term);
        } else if fallback.is_empty() {
            controls.set_feedback("no terminal text is selected");
        } else if let Err(message) = ui.send_terminal_bytes(&terminal, fallback) {
            controls.set_feedback(message);
        }
        return true;
    }
    let Some(terminal) = runtime
        .wants_live_input()
        .then(|| runtime.focused_terminal())
        .flatten()
    else {
        return false;
    };
    // Mode lookup walks the attached terminal set, so keep ordinary keystrokes
    // on their existing direct path and consult it only for a paste.
    let bracketed_paste = matches!(key, Key::Paste(_))
        && ui
            .terminal_input_modes(&terminal)
            .is_some_and(|modes| modes.paste == PasteMode::Bracketed);
    let Some(mut bytes) = key_to_terminal_bytes_for_mode(key.clone(), bracketed_paste) else {
        return false;
    };
    // A generic shell's Ctrl-C is the workspace-terminal reset gesture: first
    // interrupt the foreground job, then let readline handle Ctrl-L and repaint
    // a fresh prompt at the top. Agent CLIs keep the ordinary SIGINT byte.
    if matches!(key, Key::Quit) && !runtime.is_agent_terminal(&terminal) {
        bytes.push(12);
    }
    // The stream port is resident, so a launch in flight never drops a
    // keystroke; a genuine stream failure is surfaced instead of swallowed.
    match ui.send_terminal_bytes(&terminal, &bytes) {
        Ok(()) => {
            // Generic shells treat Ctrl-L (and the Ctrl-C reset sequence above)
            // as a user-facing clear. Mutate the local view only after the
            // durable input was accepted, matching the daemon authority.
            if matches!(bytes.as_slice(), [12] | [3, 12])
                && !runtime.is_agent_terminal(&terminal)
                && ui.clear_terminal_for_user(&terminal)
            {
                controls.reset_after_clear();
            }
        }
        Err(message) => controls.set_feedback(message),
    }
    true
}

pub(super) struct UnavailableExternalTerminalPort;

impl ExternalTerminalPort for UnavailableExternalTerminalPort {
    fn open(&mut self, _: &Path) -> Result<(), String> {
        Err("external terminal launch is unavailable".to_owned())
    }
}

/// Completion of one non-blocking Agent / terminal launch.
///
/// No port travels in the message: the launch client is shared and the resident
/// stream port never left the UI, so a completion carries only the fenced
/// identity of the operation it finishes.
pub(super) struct PaneLaunchCompletion {
    /// The admitted worker's fence, or [`PANE_LAUNCH_UNADMITTED`] for a
    /// completion no worker produced (an admission refusal).
    pub(super) launch_id: u64,
    pub(super) outcome: PaneLaunchOutcome,
}

#[derive(Clone)]
pub(super) enum PaneLaunchOutcome {
    Agent {
        operation: OperationId,
        result: Result<AgentPaneAdmission, String>,
    },
    Terminal {
        operation: OperationId,
        result: Result<TerminalRef, String>,
    },
    /// One explicit per-tab provider resume (#510).
    ResumeExact {
        operation: OperationId,
        continuation: AgentContinuationRef,
        result: Result<ExactAgentResume, String>,
    },
}

/// A pane has already been rendered as pending before this work is run.
pub(super) enum PaneLaunch {
    Agent {
        operation: OperationId,
        workspace: WorkspaceId,
        /// Absent for a workspace-root Agent.
        session: Option<SessionId>,
        profile: Option<AgentProfileId>,
        /// Present only for the opt-in workspace-root Work Run.
        goal: Option<String>,
        resume: bool,
    },
    Terminal {
        operation: OperationId,
        workspace: WorkspaceId,
        /// Absent for a workspace-root terminal.
        session: Option<SessionId>,
        arguments: String,
    },
    /// The user explicitly resumed one interrupted tab. The opaque target came
    /// from the daemon's own inventory; the TUI adds only the operation.
    ResumeExact {
        operation: OperationId,
        continuation: AgentContinuationRef,
        target: AgentResumeTarget,
    },
}

impl PaneLaunch {
    /// The identity a completion must carry to finish exactly this pending pane.
    /// It is captured before the request runs, so a panicking worker or a request
    /// refused by admission still completes that one pane.
    pub(super) fn identity(&self) -> PaneLaunchIdentity {
        match self {
            Self::Agent { operation, .. } => PaneLaunchIdentity::Agent(*operation),
            Self::Terminal { operation, .. } => PaneLaunchIdentity::Terminal(*operation),
            Self::ResumeExact {
                operation,
                continuation,
                ..
            } => PaneLaunchIdentity::ResumeExact(*operation, *continuation),
        }
    }
}

/// The fenced identity of one admitted launch, independent of its request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneLaunchIdentity {
    Agent(OperationId),
    Terminal(OperationId),
    ResumeExact(OperationId, AgentContinuationRef),
}

impl PaneLaunchIdentity {
    pub(super) fn operation(self) -> OperationId {
        match self {
            Self::Agent(operation)
            | Self::Terminal(operation)
            | Self::ResumeExact(operation, _) => operation,
        }
    }

    /// The one safe-failure completion this pane gets when its request never
    /// reached the daemon (admission refusal) or its worker died.
    pub(super) fn failed(self, message: &str) -> PaneLaunchOutcome {
        match self {
            Self::Agent(operation) => PaneLaunchOutcome::Agent {
                operation,
                result: Err(message.to_owned()),
            },
            Self::Terminal(operation) => PaneLaunchOutcome::Terminal {
                operation,
                result: Err(message.to_owned()),
            },
            Self::ResumeExact(operation, continuation) => PaneLaunchOutcome::ResumeExact {
                operation,
                continuation,
                result: Err(message.to_owned()),
            },
        }
    }
}

/// Admit one pane launch, or refuse it with the single completion its already
/// pending tab needs.
///
/// The queue is bounded: at most one worker owns the launch client and at most
/// [`PANE_LAUNCH_QUEUE_LIMIT`] further operations wait visibly pending. Beyond
/// that bound the request never reaches the daemon and completes immediately as
/// Busy, so a burst of activations can neither grow an unbounded queue nor leave
/// a pending pane without exactly one completion.
pub(super) fn enqueue_pane_launch(ui: &mut WorkspaceIoRuntime, launch: PaneLaunch) {
    if ui.pane_launches.len() >= PANE_LAUNCH_QUEUE_LIMIT {
        // Route the refusal through the same completion channel as a worker
        // result: the pending pane clears on the one path that owns it.
        let _ = ui.pane_completion_sender.send(PaneLaunchCompletion {
            launch_id: PANE_LAUNCH_UNADMITTED,
            outcome: launch.identity().failed(PANE_LAUNCH_BUSY),
        });
        return;
    }
    ui.pane_launches.push(launch);
}

/// Start one daemon launch after its pending tab has reached the terminal.
///
/// The worker borrows the shared launch client and returns only a fenced
/// [`PaneLaunchCompletion`]; the resident terminal stream port stays with the
/// live panes. A slow, hung, or panicking request therefore blocks neither input,
/// wave redraws, pane poll / input / resize / detach, nor the interaction marker
/// that suppresses automatic focus. One worker at a time keeps the launch
/// client's request sequence single-writer; the rest stay visibly pending.
pub(super) fn drain_pane_launches(ui: &mut WorkspaceIoRuntime, geometry: Geometry) {
    if ui.active_pane_launch.is_some() || ui.pane_launches.is_empty() {
        return;
    }
    let launch = ui.pane_launches.remove(0);
    let identity = launch.identity();
    let launch_id = ui.next_pane_launch;
    ui.next_pane_launch = ui.next_pane_launch.wrapping_add(1).max(PANE_LAUNCH_FIRST);
    ui.active_pane_launch = Some(launch_id);
    let port = std::sync::Arc::clone(&ui.pane_launch_commands);
    let sender = ui.pane_completion_sender.clone();
    std::thread::spawn(move || {
        // A panicking client still owes this pane one completion, and the shared
        // port survives the unwind because the worker only borrowed it.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_pane_launch(port.as_ref(), launch, geometry)
        }))
        .unwrap_or_else(|_| identity.failed(PANE_LAUNCH_WORKER_FAILED));
        let _ = sender.send(PaneLaunchCompletion { launch_id, outcome });
    });
}

/// Issue exactly one launch request over the shared client. Agent and generic
/// terminal, workspace root and session all take this single path, so they obey
/// the same ownership rule.
pub(super) fn run_pane_launch(
    port: &dyn PaneLaunchCommandPort,
    launch: PaneLaunch,
    geometry: Geometry,
) -> PaneLaunchOutcome {
    match launch {
        PaneLaunch::Agent {
            operation,
            workspace,
            session,
            profile,
            goal,
            resume,
        } => {
            let result = if resume {
                session.map_or_else(
                    || Err("workspace-root Agent resume is unavailable".to_owned()),
                    |session| port.resume(workspace, session, operation),
                )
            } else if let Some(goal) = goal {
                if session.is_some() {
                    Err("goal-driven Agent launch requires workspace-root scope".to_owned())
                } else {
                    port.launch_goal(operation, workspace, profile, &goal)
                }
            } else {
                // The pending pane's own operation is what the daemon admits and
                // finalizes, so no second identity can complete this pane (#522).
                port.launch(operation, workspace, session, profile)
            };
            PaneLaunchOutcome::Agent { operation, result }
        }
        PaneLaunch::ResumeExact {
            operation,
            continuation,
            target,
        } => PaneLaunchOutcome::ResumeExact {
            operation,
            continuation,
            result: port.resume_exact(target, operation),
        },
        PaneLaunch::Terminal {
            operation,
            workspace,
            session,
            arguments,
        } => PaneLaunchOutcome::Terminal {
            operation,
            result: port.launch_terminal(workspace, session, geometry, &arguments, operation),
        },
    }
}

/// Maps a resolved live-terminal action to its Home reducer key. Tab close and
/// terminal scroll/copy stay pane- and shell-level concerns the Home reducer has
/// no vocabulary for, so they return `None`.
pub(super) fn live_action_to_app_key(action: LiveTerminalAction) -> Option<AppKey> {
    match action {
        LiveTerminalAction::Switch => Some(AppKey::CtrlO),
        LiveTerminalAction::OpenCloseupModal => Some(AppKey::OpenCloseupOverlay),
        LiveTerminalAction::PreviousSession => Some(AppKey::PreviousSession),
        LiveTerminalAction::NextSession => Some(AppKey::NextSession),
        LiveTerminalAction::NextTab => Some(AppKey::CtrlN),
        LiveTerminalAction::PreviousTab => Some(AppKey::CtrlP),
        LiveTerminalAction::OpenPullRequests => Some(AppKey::OpenPrs),
        LiveTerminalAction::OpenPreview => Some(AppKey::OpenPreview),
        LiveTerminalAction::OpenDecisions => Some(AppKey::OpenDecisions),
        LiveTerminalAction::OpenNotes => Some(AppKey::OpenNotes),
        LiveTerminalAction::OpenGarden => Some(AppKey::OpenGarden),
        LiveTerminalAction::Agent => Some(AppKey::CtrlA),
        LiveTerminalAction::Director => Some(AppKey::ToggleDirectorDrawer),
        LiveTerminalAction::DirectorBack => Some(AppKey::DirectorBack),
        LiveTerminalAction::DirectorNew => Some(AppKey::OpenDirectorNew),
        LiveTerminalAction::WorkRuns => Some(AppKey::OpenDirectorWorkRuns),
        LiveTerminalAction::RootTerminal => Some(AppKey::ToggleRootTerminalDrawer),
        LiveTerminalAction::RootTerminalFullHeight => Some(AppKey::ToggleRootTerminalFullHeight),
        LiveTerminalAction::NewRootTerminal => Some(AppKey::OpenRootTerminal),
        LiveTerminalAction::QuitConfirmation => Some(AppKey::OpenQuitConfirmation),
        LiveTerminalAction::KeyboardHelp
        | LiveTerminalAction::OpenWorkspace
        | LiveTerminalAction::OpenWorkspaceSwitcher
        | LiveTerminalAction::ActivateWorkspace(_)
        | LiveTerminalAction::PreviousWorkspace
        | LiveTerminalAction::NextWorkspace
        | LiveTerminalAction::CloseTab
        | LiveTerminalAction::ResumeTab
        | LiveTerminalAction::MoveTabNext
        | LiveTerminalAction::MoveTabPrevious
        | LiveTerminalAction::ScrollUp
        | LiveTerminalAction::ScrollDown
        | LiveTerminalAction::ScrollBottom
        | LiveTerminalAction::Wheel { .. } => None,
    }
}

pub(super) fn terminal_geometry(height: usize, width: usize) -> Geometry {
    let (rows, cols) = super::views::workspace::terminal_viewport(height, width);
    Geometry {
        cols: u16::try_from(cols.min(usize::from(u16::MAX)))
            .expect("clamped terminal width fits u16"),
        rows: u16::try_from(rows.min(usize::from(u16::MAX)))
            .expect("clamped terminal height fits u16"),
    }
}

/// Geometry of the managed terminal underneath workspace drawers.
///
/// Director and workspace-terminal drawers are presentation overlays, like the
/// PR modal. Their open state must not resize or reflow the Home terminal they
/// cover, even when a drawer occupies its whole visible area.
pub(super) fn managed_background_terminal_geometry(height: usize, width: usize) -> Geometry {
    terminal_geometry(height, width)
}

pub(super) fn foreground_terminal_geometry(
    height: usize,
    width: usize,
    director_open: bool,
    root_terminal_open: bool,
    root_terminal_full_height: bool,
    focus: Option<WorkspaceDrawerFocus>,
) -> Geometry {
    if director_open && focus == Some(WorkspaceDrawerFocus::Director) {
        let viewport = super::director_drawer::terminal_viewport(height, width);
        Geometry {
            cols: u16::try_from(viewport.cols.min(usize::from(u16::MAX)))
                .expect("clamped drawer terminal width fits u16"),
            rows: u16::try_from(viewport.rows.min(usize::from(u16::MAX)))
                .expect("clamped drawer terminal height fits u16"),
        }
    } else if root_terminal_open {
        let available_width =
            super::views::workspace::root_terminal_available_width(height, width, director_open);
        let viewport = super::views::root_terminal_drawer::terminal_viewport_for_mode(
            height,
            width,
            available_width,
            root_terminal_full_height,
        );
        Geometry {
            cols: u16::try_from(viewport.cols.min(usize::from(u16::MAX)))
                .expect("clamped root-terminal drawer width fits u16"),
            rows: u16::try_from(viewport.rows.min(usize::from(u16::MAX)))
                .expect("clamped root-terminal drawer height fits u16"),
        }
    } else {
        terminal_geometry(height, width)
    }
}

/// Return the managed terminal underneath either workspace drawer.
///
/// A modal-like overlay never changes the background attachment merely because
/// it occludes every cell. Keeping the stream attached also preserves the
/// background screen exactly across drawer open/close transitions.
pub(super) fn managed_background_terminal(runtime: &WorkspaceRuntime) -> Option<TerminalRef> {
    runtime.workspace_drawer_background_terminal()
}

/// Stable attachment set for Home and both workspace drawers.
/// The focused root surface comes first; retained background surfaces are
/// read-only and keep their non-overlay geometry.
pub(super) fn workspace_terminal_attachments(
    runtime: &WorkspaceRuntime,
    height: usize,
    width: usize,
) -> Vec<(TerminalRef, Geometry)> {
    let mut visible = Vec::with_capacity(3);
    let mut push = |terminal: Option<TerminalRef>, geometry: Geometry| {
        if let Some(terminal) = terminal
            && !visible
                .iter()
                .any(|(shown, _): &(TerminalRef, Geometry)| shown.fences(&terminal))
        {
            visible.push((terminal, geometry));
        }
    };
    push(
        runtime.preview_terminal(),
        foreground_terminal_geometry(
            height,
            width,
            runtime.state().director_drawer_open(),
            runtime.state().root_terminal_drawer_open(),
            runtime.state().root_terminal_full_height(),
            runtime.state().workspace_drawer_focus(),
        ),
    );
    push(
        managed_background_terminal(runtime),
        managed_background_terminal_geometry(height, width),
    );
    push(
        runtime.director_terminal(),
        foreground_terminal_geometry(
            height,
            width,
            true,
            false,
            false,
            Some(WorkspaceDrawerFocus::Director),
        ),
    );
    push(
        runtime.root_terminal(),
        foreground_terminal_geometry(
            height,
            width,
            runtime.state().director_drawer_open(),
            true,
            runtime.state().root_terminal_full_height(),
            Some(WorkspaceDrawerFocus::Terminal),
        ),
    );
    visible
}

/// Project the focused live terminal's already-polled rows for
/// `with_terminal_view`, folding in the shell-owned scroll offset, selection
/// highlight, and copy feedback tracked by `controls`. Focus changes select the
/// matching terminal-local state; tabs no longer present in the registry are
/// pruned from the bounded cache.
pub(super) fn controller_terminal_view(
    ui: &WorkspaceIoRuntime,
    runtime: &WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    viewport_rows: usize,
) -> Option<TerminalViewProjection> {
    #[cfg(test)]
    TERMINAL_PROJECTION_BUILDS.set(TERMINAL_PROJECTION_BUILDS.get() + 1);
    let terminal = runtime.preview_terminal();
    let mut live_terminals = runtime.background_terminals();
    if let Some(terminal) = &terminal {
        live_terminals.push(terminal.clone());
    }
    controls.retain_terminals(&live_terminals);
    controls.sync_focus(terminal.as_ref());
    let terminal = terminal?;
    let (buffer, _, _) = ui.terminal_row_extent(&terminal, None)?;
    let selection = controls.selection_for(buffer);
    let (buffer, row_origin, total_rows) = ui.terminal_row_extent(&terminal, selection)?;
    let range = controls.visible_range(buffer, row_origin, total_rows, viewport_rows);
    let rows = ui
        .terminal_row_window(
            &terminal,
            range.start,
            range.end,
            controls.selection_for(buffer),
        )
        .expect("terminal extent guarantees the same attached row window");
    let mut projection = controls.project_window(rows, range.start, total_rows);
    if let Some(error) = ui.terminal_error(&terminal) {
        projection.feedback = Some(error.to_owned());
    } else if let Some(error) = runtime.active_pane().error() {
        projection.feedback = Some(error.to_owned());
    }
    Some(projection)
}

/// Run the per-frame visible-terminal sweep: poll the attached selection(s),
/// auto-close them if exited, then project the focused viewport. Returns
/// the projection plus its `(rows_len, scroll)` so a later pointer drag maps back
/// to the exact retained cell.
#[cfg(test)]
pub(super) fn poll_and_project_terminals(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    geometry: Geometry,
) -> (Option<TerminalViewProjection>, usize, usize) {
    close_exited_panes(ui, runtime);
    sync_terminal_selection_motions(ui, controls);
    let terminal_view = controller_terminal_view(ui, runtime, controls, usize::from(geometry.rows));
    let (rows_len, scroll) = match &terminal_view {
        Some(view) => (view.total_rows, controls.scroll()),
        None => (0, 0),
    };
    (terminal_view, rows_len, scroll)
}

pub(super) fn sync_terminal_selection_motions(
    ui: &mut WorkspaceIoRuntime,
    controls: &mut LiveTerminalControls,
) {
    for (terminal, motions) in ui.take_terminal_row_motions() {
        controls.apply_retained_row_motions(&terminal, &motions);
    }
}

/// Close every pane the daemon reports as exited, from either observation lane:
/// each attached display target's own `Resume` stream, and the bounded
/// per-scope inventory that watches the detached background tabs. The runtime
/// drops the tab (clearing `has_live_pane` when it was the last) and the shell
/// releases whatever client state it held.
///
/// Both lanes complete on their own threads, so a slow, hung, or unavailable
/// owner delays only the observation, never this frame.
pub(super) fn close_exited_panes(ui: &mut WorkspaceIoRuntime, runtime: &mut WorkspaceRuntime) {
    let background = runtime.background_terminals();
    let exited = ui
        .poll_all_terminals()
        .into_iter()
        .chain(ui.sync_background_terminals(&background))
        .collect::<Vec<_>>();
    let agent_exited = exited
        .iter()
        .any(|terminal| runtime.is_agent_terminal(terminal));
    for terminal in exited {
        let _ = runtime.exit_pane(shell_target_for_terminal(&terminal), terminal.clone());
        ui.close_terminal(&terminal);
    }
    if agent_exited {
        // The tab disappears immediately, but sidebar/Garden membership reads
        // the last coherent Agent inventory. Wake the dedicated restore lane so
        // a terminated Agent is removed there without waiting for an unrelated
        // session lifecycle change to trigger another observation.
        ui.request_agent_inventory_change_observation();
    }
}

/// The pane target a terminal ref belongs to. Mirrors the pane reducer's own
/// mapping so the shell routes an exit to the same registry entry.
pub(super) fn shell_target_for_terminal(terminal: &TerminalRef) -> Target {
    terminal
        .session_id
        .map_or(Target::Root(terminal.workspace_id), Target::Session)
}

/// Close the focused pane tab (Ctrl-O x / Ctrl-O Ctrl-X) and perform the daemon transport work:
/// request a live process exit, or drop a still-pending launch (both its queued
/// work and its completion routing) so it cannot spawn a detached daemon
/// terminal behind the vanished placeholder.
pub(super) fn close_focused_terminal_pane(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    // A live Agent is daemon-owned, so detaching its client subscription would
    // only hide a still-running process and leave its capacity occupied. Make
    // the close chord equivalent to the documented Ctrl-D exit instead. The
    // normal exit observation then removes the authoritative tab and refreshes
    // sidebar/Garden membership.
    if let Some(terminal) = runtime.focused_agent_terminal() {
        let ctrl_d = key_to_terminal_bytes_for_mode(Key::CtrlD, false)
            .expect("Ctrl-D always has a live-terminal byte encoding");
        if let Err(message) = ui.send_terminal_bytes(&terminal, &ctrl_d) {
            runtime.surface_focused_pane_feedback(message);
        }
        return;
    }
    // An interrupted Agent owns no live PTY to terminate. Persist its exact
    // lineage dismissal before changing the visible registry, so refresh,
    // reconnect, and a fresh TUI cannot resurrect the tab. The mutation also
    // creates the saved slot when this history came from inventory alone.
    if let Some(interrupted) = runtime.focused_interrupted().cloned() {
        let target = runtime
            .panes()
            .active()
            .expect("a focused interrupted tab belongs to an active target");
        dismiss_interrupted_history(ui, runtime, target, interrupted);
        return;
    }
    if let Some(terminal) = runtime.focused_terminal() {
        // Generic terminals are daemon-owned too, but closing one is an
        // explicit request to end its shell rather than merely hiding a live
        // process. SIGINT clears a possibly-running foreground command/current
        // edit before `exit` is consumed by the shell.
        if let Err(message) = ui.send_terminal_bytes(&terminal, b"\x03exit\r") {
            runtime.surface_focused_pane_feedback(message);
            return;
        }
        let outcome = runtime.close_focused_pane();
        if let Some(terminal) = outcome.detach {
            ui.close_generic_terminal(&terminal);
        }
        return;
    }
    let outcome = runtime.close_focused_pane();
    if let Some(operation) = outcome.cancel {
        pending_targets.remove(&operation);
        // Only a launch still waiting for admission is cancellable: an admitted
        // worker's request may already have reached the daemon.
        if let Some(index) = ui
            .pane_launches
            .iter()
            .position(|launch| launch.identity().operation() == operation)
        {
            ui.pane_launches.remove(index);
        }
    }
}

// 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
#[allow(clippy::too_many_arguments)]
/// Drive the complete terminal-output pointer gesture in one place. Down records
/// a snapshot and anchor without selecting, the first Drag promotes it to a text
/// selection, and Up resolves to exactly one of copy or link-open. `rows_len` /
/// `scroll` describe the frame's projected viewport so every phase maps back to
/// the exact retained cell.
pub(super) fn handle_terminal_pointer(
    ui: &WorkspaceIoRuntime,
    runtime: &WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    browser: &mut dyn BrowserOpener,
    height: usize,
    width: usize,
    rows_len: usize,
    scroll: usize,
    pointer: PointerEvent,
) -> bool {
    let point_at = |column, row| {
        if runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director) {
            super::director_drawer::terminal_point_at(height, width, rows_len, scroll, column, row)
        } else if runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Terminal) {
            super::views::root_terminal_drawer::terminal_point_at_for_mode(
                height,
                width,
                super::views::workspace::root_terminal_available_width(
                    height,
                    width,
                    runtime.state().director_drawer_open(),
                ),
                runtime.state().root_terminal_full_height(),
                rows_len,
                scroll,
                column,
                row,
            )
        } else {
            terminal_point_at(height, width, rows_len, scroll, column, row)
        }
    };
    match pointer.kind {
        PointerKind::Move => return true,
        PointerKind::Down => {
            // Live input is a session-wide level: a non-terminal tab such as
            // Workflow can be selected while another tab of the same session is
            // live. That frame has no terminal to select text in, so the press
            // belongs to the pane's own controls rather than to a PTY.
            let Some(terminal) = runtime
                .wants_live_input()
                .then(|| runtime.focused_terminal())
                .flatten()
            else {
                return false;
            };
            let Some(point) = point_at(pointer.column, pointer.row) else {
                return false;
            };
            let Some(selection) = ui.begin_terminal_selection(&terminal, point) else {
                return false;
            };
            controls.press_pointer(selection);
        }
        PointerKind::Drag => {
            if runtime.focused_terminal().is_none() {
                return true;
            }
            let Some(point) = point_at(pointer.column, pointer.row) else {
                return true;
            };
            controls.drag_pointer(point);
        }
        PointerKind::Up => match controls.release_pointer() {
            PointerRelease::Copy(text) => {
                let result = term.copy_text(&text);
                controls.record_copy(&text, result);
            }
            PointerRelease::Click => {
                let Some(terminal) = runtime.focused_terminal() else {
                    return true;
                };
                let Some(point) = point_at(pointer.column, pointer.row) else {
                    return true;
                };
                if let Some(cells) = ui.terminal_cells(&terminal) {
                    controls.open_link_at(&cells, point, browser);
                }
            }
            PointerRelease::None => {}
        },
    }
    true
}

/// Copy the retained terminal selection, if any, and leave its highlight in
/// place so the same output can be copied repeatedly.
pub(super) fn copy_terminal_selection(
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
) {
    let Some(selection) = controls.selection() else {
        controls.set_feedback("no terminal text is selected");
        return;
    };
    let text = selection.text();
    if text.is_empty() {
        controls.set_feedback("no terminal text is selected");
        return;
    }
    let result = term.copy_text(&text);
    controls.record_copy(&text, result);
}

pub(super) fn select_root_terminal_tab(key: &Key, runtime: &mut WorkspaceRuntime) -> bool {
    if !runtime.state().root_terminal_drawer_open() {
        return false;
    }
    let direction = match key {
        Key::Live(LiveTerminalAction::NextTab) => {
            crate::usecase::application::controller::TabDirection::Next
        }
        Key::Live(LiveTerminalAction::PreviousTab) => {
            crate::usecase::application::controller::TabDirection::Previous
        }
        _ => return false,
    };
    if let Some(selection) = runtime.root_terminal_selection_after_select(direction) {
        let _ = runtime.select_tab_selection(selection);
    }
    true
}

pub(super) fn select_right_pane_tab(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    index: usize,
) -> bool {
    let Some(selection) = runtime.tab_selection_at(index) else {
        return false;
    };
    let active = runtime
        .panes()
        .active()
        .expect("a selectable tab always belongs to an active pane");
    if !ui.has_agent_intent_for(active.session_id()) {
        let _ = runtime.select_tab_selection(selection);
        return true;
    }
    let continuation = match &selection {
        TabSelection::Live(terminal) => ui.agent_continuation_for(terminal),
        TabSelection::Interrupted(continuation) => Some(*continuation),
        TabSelection::Pending(_) | TabSelection::Ready(_) => None,
    };
    match ui.mutate_agent_intent(AgentTabIntentMutation::Select {
        session_id: active.session_id(),
        continuation,
    }) {
        Ok(()) => {
            let _ = runtime.select_tab_selection(selection);
            true
        }
        Err(error) => {
            surface_agent_tab_intent_error(runtime, error);
            false
        }
    }
}

// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
// 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
#[allow(clippy::too_many_arguments)]
/// Intercept the live-terminal view controls the Home reducer does not own —
/// copy, scroll, tab close, and pointer drag — returning `true` when the key was
/// consumed here so the shell loop skips reducer dispatch. `rows_len` / `scroll`
/// describe the frame's projected viewport for pointer mapping.
pub(super) fn intercept_live_terminal_control(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    browser: &mut dyn BrowserOpener,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    height: usize,
    width: usize,
    rows_len: usize,
    scroll: usize,
) -> bool {
    if runtime.state().root_terminal_drawer_open()
        && let Key::Click { column, row } = key
    {
        let projection = runtime.root_terminal_projection(None);
        if let Some(index) = super::views::root_terminal_drawer::tab_at_for_mode(
            height,
            width,
            super::views::workspace::root_terminal_available_width(
                height,
                width,
                runtime.state().director_drawer_open(),
            ),
            &projection.tabs,
            runtime.state().root_terminal_full_height(),
            *column,
            *row,
        ) {
            if let Some(selection) = runtime.root_terminal_selection_at(index) {
                let _ = runtime.select_tab_selection(selection);
            }
            return true;
        }
    }
    if is_director_new_click(key, runtime, height, width) {
        // Let the frame-loop action branch return the resulting launch effect
        // to the normal backend dispatcher.
        return false;
    }
    if is_director_new_pointer(key, runtime, height, width) {
        // The whole pointer gesture belongs to the drawer chrome. In
        // particular a second Down while Choosing and its Up are inert instead
        // of becoming picker Enter or a background terminal/pane click.
        return true;
    }
    // The right pane is interactive only on the unobscured Closeup surface.
    // Pending/ready tabs still need Closeup tab controls even though they do not
    // own PTY input yet. Consume pane-only controls while Switch or a foreground
    // overlay owns the surface so wheel/prefix/pointer events cannot mutate the
    // dimmed/covered background. Ordinary clicks still fall through to the
    // controller: its sidebar hit-test accepts the left pane and treats the
    // dimmed right pane as inert.
    let pane_only_control = if let Key::Live(action) = key {
        *action == LiveTerminalAction::ScrollUp
            || *action == LiveTerminalAction::ScrollDown
            || *action == LiveTerminalAction::ScrollBottom
            || matches!(action, LiveTerminalAction::Wheel { .. })
            || *action == LiveTerminalAction::CloseTab
            || *action == LiveTerminalAction::ResumeTab
            || *action == LiveTerminalAction::MoveTabNext
            || *action == LiveTerminalAction::MoveTabPrevious
    } else {
        matches!(key, Key::Pointer(_))
    };
    if !runtime.wants_pane_control_input() && pane_only_control {
        return true;
    }
    if !select_director_tab_and_activate(key, ui, runtime, pending_targets)
        && !select_root_terminal_tab(key, runtime)
    {
        match key {
            Key::Live(LiveTerminalAction::ScrollUp) => controls.scroll_up(),
            Key::Live(LiveTerminalAction::ScrollDown) => controls.scroll_down(),
            Key::Live(LiveTerminalAction::ScrollBottom) => controls.scroll_to_bottom(),
            Key::Live(LiveTerminalAction::Wheel {
                up,
                column,
                row,
                notches,
            }) => {
                let point = if runtime.state().workspace_drawer_focus()
                    == Some(WorkspaceDrawerFocus::Director)
                {
                    super::director_drawer::terminal_point_at(height, width, 0, 0, *column, *row)
                } else if runtime.state().workspace_drawer_focus()
                    == Some(WorkspaceDrawerFocus::Terminal)
                {
                    super::views::root_terminal_drawer::terminal_point_at_for_mode(
                        height,
                        width,
                        super::views::workspace::root_terminal_available_width(
                            height,
                            width,
                            runtime.state().director_drawer_open(),
                        ),
                        runtime.state().root_terminal_full_height(),
                        0,
                        0,
                        *column,
                        *row,
                    )
                } else {
                    terminal_point_at(height, width, 0, 0, *column, *row)
                };
                let Some(point) = point else {
                    return true;
                };
                let Some(terminal) = runtime.focused_terminal() else {
                    return true;
                };
                let Some(modes) = ui.terminal_input_modes(&terminal) else {
                    return true;
                };
                let bytes = if modes.mouse_protocol {
                    Some(
                        encode_mouse_wheel(*up, point.column, point.row, modes.mouse_encoding)
                            .repeat(*notches),
                    )
                } else if modes.alternate_screen {
                    Some(encode_wheel_arrows(*up, modes.application_cursor).repeat(*notches))
                } else {
                    None
                };
                if let Some(bytes) = bytes {
                    if let Err(message) = ui.send_terminal_bytes(&terminal, &bytes) {
                        controls.set_feedback(message);
                    }
                } else {
                    let lines = notches.saturating_mul(WHEEL_LINES);
                    if *up {
                        controls.scroll_up_by(lines);
                    } else {
                        controls.scroll_down_by(lines);
                    }
                }
            }
            Key::Live(LiveTerminalAction::CloseTab) => {
                close_focused_terminal_pane(ui, runtime, pending_targets);
            }
            Key::Live(LiveTerminalAction::ResumeTab) => {
                activate_focused_interrupted_tab(ui, runtime, pending_targets);
            }
            Key::Live(
                action @ (LiveTerminalAction::MoveTabNext | LiveTerminalAction::MoveTabPrevious),
            ) => {
                if runtime.state().root_terminal_drawer_open() {
                    return true;
                }
                let direction = if *action == LiveTerminalAction::MoveTabNext {
                    crate::usecase::application::controller::TabDirection::Next
                } else {
                    crate::usecase::application::controller::TabDirection::Previous
                };
                let (current_tabs, next_tabs) = runtime.tab_order_after_reorder(direction);
                let mut current = Vec::new();
                for selection in current_tabs {
                    match selection {
                        TabSelection::Live(terminal) => {
                            if let Some(continuation) = ui.agent_continuation_for(&terminal) {
                                current.push(continuation);
                            }
                        }
                        TabSelection::Interrupted(continuation) => current.push(continuation),
                        TabSelection::Pending(_) | TabSelection::Ready(_) => {}
                    }
                }
                let mut continuations = Vec::new();
                for selection in next_tabs {
                    match selection {
                        TabSelection::Live(terminal) => {
                            if let Some(continuation) = ui.agent_continuation_for(&terminal) {
                                continuations.push(continuation);
                            }
                        }
                        TabSelection::Interrupted(continuation) => continuations.push(continuation),
                        TabSelection::Pending(_) | TabSelection::Ready(_) => {}
                    }
                }
                let persisted = if current == continuations {
                    Ok(())
                } else {
                    ui.mutate_agent_intent(AgentTabIntentMutation::Reorder {
                        session_id: runtime.panes().active().and_then(Target::session_id),
                        continuations,
                    })
                };
                match persisted {
                    Ok(()) => {
                        let _ = runtime.reorder_tab(direction);
                    }
                    Err(error) => surface_agent_tab_intent_error(runtime, error),
                }
            }
            Key::Pointer(pointer) => {
                return handle_terminal_pointer(
                    ui, runtime, controls, term, browser, height, width, rows_len, scroll, *pointer,
                );
            }
            Key::Click { column, row } => {
                return handle_terminal_pointer(
                    ui,
                    runtime,
                    controls,
                    term,
                    browser,
                    height,
                    width,
                    rows_len,
                    scroll,
                    PointerEvent {
                        kind: PointerKind::Down,
                        column: *column,
                        row: *row,
                    },
                );
            }
            _ => return false,
        }
    }
    true
}

/// Apply completed pane launches: promote and focus the runtime tab, then attach
/// the daemon terminal stream, so the live viewport renders next frame.
///
/// A completion frees the launch admission slot only when its fence matches the
/// admitted worker, so a duplicate, late, or unadmitted (Busy) completion cannot
/// release a newer worker's slot. Which pending pane it applies to remains fenced
/// by `pending_targets` and the runtime's own operation identity.
pub(super) fn drain_pane_completions_into_runtime(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    _geometry: Geometry,
) {
    let completions = ui
        .pane_completions
        .try_iter()
        .take(FRAME_EVENT_BUDGET)
        .collect::<Vec<_>>();
    for completion in completions {
        if ui.active_pane_launch == Some(completion.launch_id) {
            ui.active_pane_launch = None;
        }
        match completion.outcome {
            PaneLaunchOutcome::Agent { operation, result } => {
                apply_agent_launch_completion(ui, runtime, pending_targets, operation, result);
            }
            PaneLaunchOutcome::ResumeExact {
                operation,
                continuation,
                result,
            } => {
                if result.is_ok() {
                    ui.request_agent_inventory_change_observation();
                }
                let Some(target) = pending_targets.remove(&operation) else {
                    continue;
                };
                apply_exact_resume(ui, runtime, target, operation, continuation, result);
            }
            PaneLaunchOutcome::Terminal { operation, result } => {
                let Some(target) = pending_targets.remove(&operation) else {
                    continue;
                };
                complete_terminal_launch(ui, runtime, target, operation, result);
            }
        }
    }
}

pub(super) fn complete_terminal_launch(
    ui: &WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    target: Target,
    operation: OperationId,
    result: Result<TerminalRef, String>,
) {
    match result {
        Ok(terminal) => {
            if ui.generic_terminal_is_closing(&terminal) {
                fail_terminal_launch(
                    runtime,
                    target,
                    operation,
                    "terminal is still closing; try again".to_owned(),
                );
                return;
            }
            let _ = runtime.complete_pane_focus_if_uninterrupted(target, operation, terminal);
        }
        Err(message) => fail_terminal_launch(runtime, target, operation, message),
    }
}

/// Finish a terminal placeholder and surface its display-safe reason in the
/// frontmost error dialog. The pane keeps the same reason as fallback feedback,
/// while the controller owns modal input and dismissal.
pub(super) fn fail_terminal_launch(
    runtime: &mut WorkspaceRuntime,
    target: Target,
    operation: OperationId,
    message: String,
) {
    let _ = runtime.fail_pane(target, operation, message.clone());
    let _ = runtime.apply_event(AppEvent::TerminalLaunchFailed(Notice::new(message)));
}

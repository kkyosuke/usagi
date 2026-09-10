//! TUI 面の presentation 層。画面描画（各画面の view・共通 widget）と
//! キー入力のマッピングを置く。描画は自前の差分レンダリングで行い、
//! UI フレームワーク（ratatui 等）には依存しない。
//! 実 IO は持たず、出力先は呼び出し側（合成ルート）から注入する。
//!
//! 描画は 3 つに分ける: 各画面の view（[`views`]）・再利用 UI 部品（[`widgets`]）・
//! 領域配置（[`layouts`]）。view が layout で領域を割り、そこへ widget を配置する。
//! 色は [`theme`] が意味的な役割で一元管理する（役割→具体色の単一情報源）。

pub mod frame;
pub mod layouts;
pub mod live_terminal;
pub mod theme;
pub mod views;
pub mod widgets;
pub mod workspace_deck;
pub mod workspace_runtime;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use chrono::{DateTime, Timelike, Utc};
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::{
    AgentInventory, AgentProfileId, AgentResumeTarget, AgentRuntimeInventoryState,
    AgentWorkspaceObservation, ProviderResumeProjection,
};
use usagi_core::domain::id::{
    AgentContinuationRef, AgentRuntimeId, OperationId, RequestId, SessionId, TerminalRef,
    UserDecisionId, WorkspaceId,
};
use usagi_core::domain::recent::Recent;
use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
use usagi_core::domain::settings::{IconMode, WorkMode};
use usagi_core::domain::supervisor::{MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS, SupervisorRunId};
use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
use usagi_core::domain::user_decision::UserDecisionAnswer;
use usagi_core::domain::workspace::Workspace;
use usagi_core::usecase::env::EnvScope;
use usagi_core::usecase::vt_screen::RetainedRowMotion;

use crate::presentation::live_terminal::{LiveTerminalControls, PointerRelease};
use crate::presentation::theme::{Color, Style};
use crate::presentation::views::config::{self, AvailableAgentModels, Config};
use crate::presentation::views::create_session_error_modal;
use crate::presentation::views::director_drawer::{
    self, DirectorConversation, DirectorDrawerProjection, DirectorNewProjection,
    DirectorOrganizationRow, WorkRunControlProjection,
};
use crate::presentation::views::key_help::{self, Context as KeyHelpContext};
use crate::presentation::views::new::{self, DirectoryCompletion, Field, New};
use crate::presentation::views::open::{self, Open};
use crate::presentation::views::pr_modal;
use crate::presentation::views::quit_modal;
use crate::presentation::views::root_terminal_drawer;
use crate::presentation::views::scratchpad_modal;
use crate::presentation::views::splash;
use crate::presentation::views::welcome::{self, MenuAction, Welcome};
use crate::presentation::views::work_run::WorkRunProjection;
use crate::presentation::views::workspace::{
    self, HomeHeaderAction, HomeProjection, ProjectedSession, TerminalViewProjection,
    Workspace as WorkspaceView, garden_click_at, garden_fits, home_header_action_at, render_home,
    render_home_at, right_pane_tab_at, terminal_point_at,
};
use crate::presentation::widgets::modal::{self, ConfirmationView};
use crate::presentation::workspace_deck::{
    OverlayIntent, ProjectBarTarget, WorkspaceDeck, project_bar, render_overlay,
};
use crate::presentation::workspace_runtime::{
    InterruptedRemovalConfirmation, PaneRestoreTarget, WorkspaceRuntime,
};
use crate::usecase::application::agent_tab_intent::{
    AgentTabIntent, AgentTabIntentError, AgentTabIntentMutation, AgentTabIntentPort,
    AgentTabIntentPortCommit, AgentTabProjection,
};
use crate::usecase::application::controller::{
    AppEvent, AppKey, AppState, BackendEvent, BranchChoice, DecisionOverlayState,
    DirectorConsoleParent, DirectorNew, DirectorRoute, Effect, EnvironmentEntry, ExitChoice,
    Feedback, GardenClick, HomeMode, NewRequest, Notice, OperationResult, Overlay, PendingToken,
    PreviewFileFilter, RoleChoice, Route, SessionBranchCatalog, SessionRoleCatalog,
    SessionRoleProjection, Target, WorkspaceDrawerFocus,
};
#[cfg(test)]
use crate::usecase::application::controller::{SafeError, SafeMessage};
use crate::usecase::application::daemon_backend::{
    AgentPort as BackendAgentPort, Completions, CreateSessionRequest, DaemonBackend,
    DecisionPort as BackendDecisionPort, Flow as BackendFlow, LaunchAgentRequest,
    OpenTerminalRequest, OverlayPort as BackendOverlayPort, RemoveSessionRequest,
    ReopenAgentRequest, ResumeAgentRequest, SessionCommandPort as BackendSessionCommandPort,
    SleepSessionRequest, TargetStorePort as BackendTargetStorePort,
    WorkspaceCommandPort as BackendWorkspaceCommandPort,
};
use crate::usecase::application::interrupted_tab::{InterruptedTab, ResumeCommand};
use crate::usecase::application::metrics::{
    GitDiff, MetricsBackend, MetricsPort, MetricsPortFactory, MetricsProjection,
};
use crate::usecase::application::observation_lane::ObservationLane;
use crate::usecase::application::pane::{PaneKind, PaneRegistry, PaneTab, TabSelection};
use crate::usecase::application::pane_runtime::Geometry;
use crate::usecase::application::pr::{BrowserOpener, PrSnapshotPort};
use crate::usecase::application::terminal_screen::{PasteMode, TerminalBuffer, TerminalInputModes};
use crate::usecase::application::terminal_selection::{TerminalPoint, TerminalSelection};
use crate::usecase::application::terminal_session::{
    SessionState, TerminalAttach, TerminalChunk, TerminalError, TerminalInputOutcome,
    TerminalInputResolution, TerminalSession, TerminalStreamPort, TerminalSubscription,
};
use crate::usecase::application::work_run_control::{
    WORK_RUN_ACTION_UNCONFIRMED, WorkRunControl, WorkRunControlAction, WorkRunControlError,
    WorkRunControlMode, WorkRunControlOutcome, WorkRunControlRequest, WorkRunControlResult,
    WorkRunPort,
};
use crate::usecase::application::{Key, ScreenRunner, Terminal, open_failure_notice};
use crate::usecase::overview::SessionCommand;
use crate::usecase::terminal_input::{
    LiveTerminalAction, PointerEvent, PointerKind, WHEEL_LINES, encode_mouse_wheel,
    encode_wheel_arrows,
};
use usagi_core::usecase::settings::SettingsPort;

#[cfg(test)]
use crate::usecase::application::WorkspaceCreateCompletion;
use crate::usecase::application::agent_runtime_ports::{
    AgentCommandPort, AgentCommandPortFactory, AgentPaneAdmission, ExactAgentResume,
    PaneLaunchCommandPort, SerializedPaneLaunchPort, TerminalCommandPort,
};
use crate::usecase::application::runtime_ports::{
    DecisionCommandPort, DesktopNotificationPort, EnvironmentStorePort, ExternalTerminalPort,
    GardenInventoryPort, RestoreConnectionPort, SessionCommandPort, SessionCommandPortFactory,
    SessionCommandResult, SessionRefreshPort, SessionWorktreeScanPort,
};
use crate::usecase::application::{
    WorkspaceCreateEffect, WorkspaceCreateToken, WorkspaceLoader, WorkspaceSnapshot,
    runtime_identities_are_valid,
};

/// Keeps an embedder without a daemon launch client safe: every pane launch
/// becomes one inline failure and nothing is spawned locally.
struct UnavailablePaneLaunchPort;

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

#[cfg(test)]
struct NoDesktopNotifications;
#[cfg(test)]
impl DesktopNotificationPort for NoDesktopNotifications {
    fn notify(&mut self, _: &str, _: &str) {}
}

/// Bridges the workspace [`AgentCommandPort`] into the [`TerminalStreamPort`]
/// expected by a [`TerminalSession`], so the session coordinator stays free of
/// the wider Agent launch vocabulary.
struct AgentStreamPort<'a, T: TerminalCommandPort + ?Sized>(&'a mut T);

impl<T: TerminalCommandPort + ?Sized> TerminalStreamPort for AgentStreamPort<'_, T> {
    fn connection_epoch(&self) -> Option<u64> {
        self.0.connection_epoch()
    }

    fn resize(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<Geometry, TerminalError> {
        self.0.resize(terminal, geometry)
    }

    fn attach(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
    ) -> Result<TerminalAttach, TerminalError> {
        self.0.attach(terminal, geometry)
    }
    fn poll(
        &mut self,
        terminal: &TerminalRef,
        after_offset: u64,
    ) -> Result<Vec<TerminalChunk>, TerminalError> {
        self.0.poll(terminal, after_offset)
    }
    fn input(
        &mut self,
        terminal: &TerminalRef,
        subscription: TerminalSubscription,
        input_seq: u64,
        operation: OperationId,
        bytes: &[u8],
    ) -> Result<TerminalInputOutcome, TerminalError> {
        self.0
            .input(terminal, subscription, input_seq, operation, bytes)
    }
    fn input_outcome(
        &mut self,
        terminal: &TerminalRef,
        operation: OperationId,
        input_len: usize,
    ) -> Result<TerminalInputResolution, TerminalError> {
        self.0.input_outcome(terminal, operation, input_len)
    }
    fn detach(&mut self, terminal: &TerminalRef, subscription: TerminalSubscription) {
        self.0.detach(terminal, subscription);
    }
}

/// Maps a management [`Key`] to the bytes a focused live terminal should
/// receive. Reserved prefix actions ([`Key::Live`]) do not reach the shell;
/// all other keys, including global controls, do while Closeup owns the pane.
#[cfg(test)]
fn key_to_terminal_bytes(key: Key) -> Option<Vec<u8>> {
    key_to_terminal_bytes_for_mode(key, false)
}

fn key_to_terminal_bytes_for_mode(key: Key, bracketed_paste: bool) -> Option<Vec<u8>> {
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
fn forward_live_terminal_input(
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

/// The frontmost workspace surface that owns one input before PTY forwarding,
/// pane controls, and the Home reducer get a chance to observe it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceForegroundInputOwner {
    /// The Director CLI picker, including its launch-pending projection, is an
    /// exclusive owner. Its small reserved-key vocabulary is reduced locally;
    /// every other user input is consumed inertly.
    DirectorPicker,
    /// No exclusive foreground surface owns the input at this routing seam.
    Downstream,
}

fn workspace_foreground_input_owner(runtime: &WorkspaceRuntime) -> WorkspaceForegroundInputOwner {
    if runtime.state().overlay().is_none()
        && runtime.state().director_drawer_open()
        && runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director)
        && (runtime.state().director_launching().is_some()
            || !matches!(runtime.state().director_new(), DirectorNew::Idle)
            || !matches!(runtime.state().director_route(), DirectorRoute::Console(_)))
    {
        WorkspaceForegroundInputOwner::DirectorPicker
    } else {
        WorkspaceForegroundInputOwner::Downstream
    }
}

/// Route an input owned by the Director picker. Resize and runtime wake events
/// are not user input and keep flowing so geometry and backend progress cannot
/// stall behind the foreground owner.
fn handle_director_picker_input(runtime: &mut WorkspaceRuntime, key: &Key) -> Option<Vec<Effect>> {
    if workspace_foreground_input_owner(runtime) == WorkspaceForegroundInputOwner::DirectorPicker {
        // Organization is management-owned, but its Director selector lives at
        // the pane seam because changing a row also persists exact Agent intent.
        // Let only those navigation keys reach `select_director_tab`; every
        // other non-Console input remains exclusive to the drawer.
        if matches!(runtime.state().director_new(), DirectorNew::Idle)
            && runtime.state().director_route() == DirectorRoute::Organization
            && matches!(
                key,
                Key::Up
                    | Key::Down
                    | Key::Live(LiveTerminalAction::PreviousTab | LiveTerminalAction::NextTab)
            )
        {
            return None;
        }
        return match key {
            Key::Resize | Key::Other => None,
            _ => Some(runtime.handle_key(key.clone())),
        };
    }
    // Console is not exclusive because its selected root Agent owns ordinary
    // terminal input. Its drawer-local open/close operations still precede PTY
    // forwarding and the Home reducer.
    if runtime.state().overlay().is_none()
        && runtime.state().director_drawer_open()
        && (matches!(
            key,
            Key::Live(LiveTerminalAction::Director | LiveTerminalAction::DirectorNew)
        ) || (matches!(key, Key::Escape) && !drawer_agent_owns_escape(runtime)))
    {
        Some(runtime.handle_key(key.clone()))
    } else {
        None
    }
}

struct WorkRunControlInput {
    outcome: WorkRunControlOutcome,
    effects: Vec<Effect>,
}

#[cfg(test)]
fn handle_work_run_control_input(
    runtime: &mut WorkspaceRuntime,
    control: &mut WorkRunControl,
    runs: &WorkRunProjection,
    key: &Key,
) -> Option<WorkRunControlInput> {
    handle_work_run_control_input_with_ui(None, runtime, control, runs, key)
}

fn handle_work_run_control_input_with_ui(
    ui: Option<&mut WorkspaceIoRuntime>,
    runtime: &mut WorkspaceRuntime,
    control: &mut WorkRunControl,
    runs: &WorkRunProjection,
    key: &Key,
) -> Option<WorkRunControlInput> {
    let can_activate = runtime.state().overlay().is_none()
        && runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven
        && runtime.state().director_launching().is_none()
        && matches!(runtime.state().director_new(), DirectorNew::Idle);
    let direct_work_runs = matches!(key, Key::Live(LiveTerminalAction::WorkRuns));
    let effects = if can_activate && direct_work_runs {
        runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorWorkRuns))
    } else {
        Vec::new()
    };
    if runtime.state().overlay().is_none()
        && runtime.state().workspace_drawer_focus() != Some(WorkspaceDrawerFocus::Terminal)
        && control.mode() == WorkRunControlMode::Submitting
        && matches!(key, Key::Live(LiveTerminalAction::DirectorNew))
    {
        return Some(WorkRunControlInput {
            outcome: WorkRunControlOutcome::Consumed,
            effects,
        });
    }
    let eligible = can_activate
        && runtime.state().director_drawer_open()
        && runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director);
    if !eligible {
        if control.mode() != WorkRunControlMode::Submitting {
            control.suspend();
        }
        return None;
    }
    if direct_work_runs {
        control.open(runs.runs());
        return Some(WorkRunControlInput {
            outcome: WorkRunControlOutcome::Consumed,
            effects,
        });
    }
    let route = runtime.state().director_route();
    let run_surface = matches!(
        route,
        DirectorRoute::WorkRuns | DirectorRoute::RunOverview(_)
    );
    if !run_surface {
        if control.mode() != WorkRunControlMode::Submitting {
            control.suspend();
        }
        return None;
    }
    if control.mode() == WorkRunControlMode::Closed {
        control.open(runs.runs());
    }
    if let DirectorRoute::RunOverview(run_id) = route {
        control.focus(run_id, runs.runs());
    }
    if matches!(key, Key::Resize | Key::Other) {
        return None;
    }
    if matches!(
        key,
        Key::Live(LiveTerminalAction::Director | LiveTerminalAction::DirectorNew)
    ) {
        control.suspend();
        return None;
    }
    if control.mode() == WorkRunControlMode::List {
        return Some(handle_work_run_list_input(
            ui, runtime, control, runs, key, route, effects,
        ));
    }
    let action = match key {
        Key::Up => WorkRunControlAction::Up,
        Key::Down => WorkRunControlAction::Down,
        Key::Left => WorkRunControlAction::PreviousDecision,
        Key::Right => WorkRunControlAction::NextDecision,
        Key::Enter => WorkRunControlAction::Enter,
        Key::Escape | Key::Live(LiveTerminalAction::DirectorBack) => WorkRunControlAction::Escape,
        Key::Quit => WorkRunControlAction::Cancel,
        _ => {
            return Some(WorkRunControlInput {
                outcome: WorkRunControlOutcome::Consumed,
                effects,
            });
        }
    };
    Some(WorkRunControlInput {
        outcome: control.handle(
            action,
            runs.runs(),
            runs.freshness() == crate::presentation::views::work_run::WorkRunFreshness::Fresh,
        ),
        effects,
    })
}

fn handle_work_run_list_input(
    ui: Option<&mut WorkspaceIoRuntime>,
    runtime: &mut WorkspaceRuntime,
    control: &mut WorkRunControl,
    runs: &WorkRunProjection,
    key: &Key,
    route: DirectorRoute,
    mut effects: Vec<Effect>,
) -> WorkRunControlInput {
    let (): () = match key {
        Key::Enter => match route {
            DirectorRoute::WorkRuns => {
                if let Some(run) = control.selected() {
                    effects.extend(
                        runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorRunOverview(run))),
                    );
                } else {
                    control.set_feedback("No Work Run is selected");
                }
            }
            DirectorRoute::RunOverview(run_id) => {
                let root = runs
                    .runs()
                    .iter()
                    .find(|run| run.supervisor_run_id == run_id)
                    .and_then(|run| run.root_agent_id);
                if root.is_some_and(|root| {
                    ui.is_some_and(|ui| select_director_agent(root, ui, runtime))
                }) {
                    effects.extend(runtime.apply_event(AppEvent::Key(
                        AppKey::OpenDirectorConsole(DirectorConsoleParent::RunOverview(run_id)),
                    )));
                } else {
                    control.set_feedback("Director Console is not available for this Work Run");
                }
            }
            DirectorRoute::Organization | DirectorRoute::Console(_) => {}
        },
        Key::Escape if route == DirectorRoute::WorkRuns => {
            effects.extend(runtime.apply_event(AppEvent::Key(AppKey::Escape)));
        }
        Key::Escape | Key::Live(LiveTerminalAction::DirectorBack) => {
            effects.extend(runtime.apply_event(AppEvent::Key(AppKey::DirectorBack)));
        }
        _ => {
            let action = match key {
                Key::Up => Some(WorkRunControlAction::Up),
                Key::Down => Some(WorkRunControlAction::Down),
                Key::Quit => Some(WorkRunControlAction::Cancel),
                Key::CtrlX => Some(WorkRunControlAction::Delete),
                _ => None,
            };
            if let Some(action) = action {
                let _ = control.handle(
                    action,
                    runs.runs(),
                    runs.freshness()
                        == crate::presentation::views::work_run::WorkRunFreshness::Fresh,
                );
            }
        }
    };
    WorkRunControlInput {
        outcome: WorkRunControlOutcome::Consumed,
        effects,
    }
}

fn select_director_agent(
    runtime_id: AgentRuntimeId,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    let Some(index) = runtime.agent_tab_index(runtime_id, ui.agent_inventory()) else {
        return false;
    };
    let selection = runtime
        .tab_selection_at(index)
        .expect("an Agent index is returned only for a tab in the same pane");
    select_director_selection(selection, ui, runtime)
}

fn select_director_selection(
    selection: TabSelection,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    let continuation = match &selection {
        TabSelection::Live(terminal) => ui.agent_continuation_for(terminal),
        TabSelection::Interrupted(continuation) => Some(*continuation),
        TabSelection::Pending(_) | TabSelection::Ready(_) => return false,
    };
    if ui
        .mutate_agent_intent(AgentTabIntentMutation::Select {
            session_id: None,
            continuation,
        })
        .is_err()
    {
        return false;
    }
    let _ = runtime.select_tab_selection(selection);
    true
}

fn work_run_control_projection(control: &WorkRunControl) -> WorkRunControlProjection {
    WorkRunControlProjection {
        mode: control.mode(),
        selected: control.selected(),
        decision: control.decision(),
        feedback: control.feedback().map(str::to_owned),
    }
}

/// Whether the drawer's selected root Agent, not the drawer itself, owns `Esc`.
///
/// An agent CLI reads `Esc` as its own interrupt / dismiss, so swallowing it to
/// close the drawer made that key unreachable for every conversation. The
/// drawer keeps `Esc` only when no live conversation can receive it — where
/// closing is the only thing left for it to mean — and `Ctrl-O Ctrl-G` still
/// closes the drawer with a live Agent attached.
fn drawer_agent_owns_escape(runtime: &WorkspaceRuntime) -> bool {
    runtime.wants_live_input() && runtime.focused_terminal().is_some()
}

/// Retarget `Ctrl-O` follow-ups whose meaning differs in an open drawer.
///
/// A root terminal drawer maps both `Ctrl-O n` and `Ctrl-O Ctrl-N` to a new
/// terminal tab. The classifier has already normalized the optional Control
/// modifier before this context-specific retargeting.
fn retarget_drawer_chords(runtime: &WorkspaceRuntime, key: Key) -> Key {
    if runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Terminal)
        && matches!(key, Key::Live(LiveTerminalAction::DirectorNew))
    {
        return Key::Live(LiveTerminalAction::NewRootTerminal);
    }
    key
}

#[derive(Debug, PartialEq, Eq)]
enum WorkspaceInputRoute {
    Drawer(Vec<Effect>),
    Garden(Vec<Effect>),
    Forwarded,
    Unhandled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrModalClickRoute {
    Inside,
    Outside,
}

fn route_pr_modal_click(
    overlay: Option<Overlay>,
    height: usize,
    width: usize,
    column: u16,
    row: u16,
) -> Option<PrModalClickRoute> {
    (overlay == Some(Overlay::Prs)).then(|| {
        if pr_modal::contains(height, width, column, row) {
            PrModalClickRoute::Inside
        } else {
            PrModalClickRoute::Outside
        }
    })
}

/// Give the PR modal ownership of a project-bar click before the process-level
/// bar can activate the surface behind it. Project-bar coordinates are outside
/// Home, so they cannot flow through [`route_pr_modal_click`]'s modal geometry.
fn dismiss_pr_modal_on_project_bar_click(runtime: &mut WorkspaceRuntime, key: &Key) -> bool {
    if runtime.state().overlay() != Some(Overlay::Prs) || !matches!(key, Key::Click { row: 0, .. })
    {
        return false;
    }
    let _ = runtime.apply_event(AppEvent::Key(AppKey::Escape));
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GardenProjectVisit {
    workspace: WorkspaceId,
    session: SessionId,
    agent: Option<AgentRuntimeId>,
}

#[derive(Debug, PartialEq, Eq)]
enum GardenInputRoute {
    Local(Vec<Effect>),
    Agent(Vec<Effect>),
    Project(GardenProjectVisit),
}

fn garden_scroll_input(material: Option<&HomeFrameMaterial>, key: &Key) -> Option<GardenClick> {
    let scroll = |lines, position| {
        material
            .and_then(|material| {
                views::workspace::garden_scroll_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    lines,
                    position,
                )
            })
            .unwrap_or(GardenClick::Dismiss)
    };
    let page = material.map_or(1, |material| {
        isize::try_from(material.height.saturating_sub(3)).unwrap_or(isize::MAX)
    });
    Some(match key {
        Key::Up => scroll(-1, None),
        Key::Down => scroll(1, None),
        Key::PageUp => scroll(-page, None),
        Key::PageDown => scroll(page, None),
        Key::Live(LiveTerminalAction::Wheel {
            up,
            column,
            row,
            notches,
        }) => {
            let lines = isize::try_from(*notches).unwrap_or(isize::MAX);
            scroll(if *up { -lines } else { lines }, Some((*column, *row)))
        }

        _ => return None,
    })
}

/// Give an open Garden exclusive ownership of the next user interaction.
/// Backend/frame ticks are not user activity and keep the screen saver open.
/// A pointer press also fences the rest of its drag/release gesture so closing
/// the Garden cannot leak that tail into the terminal selection underneath.
fn route_garden_input(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    material: Option<&HomeFrameMaterial>,
    key: &Key,
    pointer_gesture: &mut bool,
) -> Option<GardenInputRoute> {
    if *pointer_gesture && matches!(key, Key::Pointer(_)) {
        if matches!(
            key,
            Key::Pointer(PointerEvent {
                kind: PointerKind::Up,
                ..
            })
        ) {
            *pointer_gesture = false;
        }
        return Some(GardenInputRoute::Local(Vec::new()));
    }
    if runtime.state().overlay() != Some(Overlay::Garden) || !is_user_activity(key) {
        return None;
    }

    if let Some(click) = garden_scroll_input(material, key) {
        return Some(GardenInputRoute::Local(
            runtime.apply_event(AppEvent::GardenClick(click)),
        ));
    }
    let pointer = match key {
        Key::Click { column, row } => material
            .and_then(|material| {
                garden_click_at(
                    material.height,
                    material.width,
                    &material.projection,
                    material.now,
                    *column,
                    *row,
                )
            })
            .unwrap_or(GardenClick::Dismiss),
        Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => {
            *pointer_gesture = true;
            material
                .and_then(|material| {
                    garden_click_at(
                        material.height,
                        material.width,
                        &material.projection,
                        material.now,
                        *column,
                        *row,
                    )
                })
                .unwrap_or(GardenClick::Dismiss)
        }
        Key::Pointer(PointerEvent {
            kind: PointerKind::Drag,
            ..
        }) => {
            *pointer_gesture = true;
            GardenClick::Dismiss
        }
        _ => GardenClick::Dismiss,
    };
    let effects = runtime.apply_event(AppEvent::GardenClick(pointer));
    if let GardenClick::Visit {
        workspace,
        session,
        agent,
    } = pointer
        && workspace != runtime.state().workspace()
    {
        return Some(GardenInputRoute::Project(GardenProjectVisit {
            workspace,
            session,
            agent,
        }));
    }
    Some(if visit_garden_agent(ui, runtime, pointer) {
        GardenInputRoute::Agent(effects)
    } else {
        GardenInputRoute::Local(effects)
    })
}

fn route_workspace_input_before_reducer(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    controls: &mut LiveTerminalControls,
    term: &mut dyn Terminal,
    key: &Key,
) -> WorkspaceInputRoute {
    if let Some(effects) = handle_director_picker_input(runtime, key) {
        WorkspaceInputRoute::Drawer(effects)
    } else if forward_live_terminal_input(ui, runtime, controls, term, key) {
        WorkspaceInputRoute::Forwarded
    } else {
        WorkspaceInputRoute::Unhandled
    }
}

fn garden_shell_owned_wake(key: &Key) -> bool {
    // List input needs the drawn viewport before it can either scroll or wake
    // a narrow Garden. Leave it for route_garden_input, just like hit-tested clicks.
    !matches!(
        key,
        Key::Click { .. }
            | Key::Up
            | Key::Down
            | Key::PageUp
            | Key::PageDown
            | Key::Live(LiveTerminalAction::Wheel { .. })
            | Key::Other
            | Key::Resize
    )
}

struct NoMetrics;
impl MetricsPort for NoMetrics {}

/// Actions whose stateful host remains in the terminal loop while
/// [`DaemonBackend`] is the sole controller-effect dispatcher.
pub enum ControllerHostAction {
    Create(CreateSessionRequest, Completions),
    Refresh(WorkspaceId, Completions),
    Remove(RemoveSessionRequest, Completions),
    Sleep(SleepSessionRequest, Completions),
    LaunchAgent(LaunchAgentRequest),
    ResumeAgent(ResumeAgentRequest),
    ReopenAgent(ReopenAgentRequest),
    OpenTerminal(OpenTerminalRequest),
    OpenExternalTerminal(Target),
    SelectTab(crate::usecase::application::controller::TabDirection),
}

/// Cloneable adapter handed to the production backend factory. It contains no
/// policy: each port call enqueues exactly one action for the terminal host.
#[derive(Clone)]
pub struct ControllerHost(Sender<ControllerHostAction>);

impl ControllerHost {
    /// Create the host adapter and the terminal loop's action receiver.
    #[must_use]
    pub fn channel() -> (Self, Receiver<ControllerHostAction>) {
        let (sender, receiver) = mpsc::channel();
        (Self(sender), receiver)
    }
}

impl BackendSessionCommandPort for ControllerHost {
    fn create(&mut self, request: CreateSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Create(request, completions))) = self
            .0
            .send(ControllerHostAction::Create(request, completions))
        {
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: request.token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new("session command host is unavailable")),
            }));
        }
    }

    fn refresh(&mut self, workspace: WorkspaceId, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Refresh(_, completions))) = self
            .0
            .send(ControllerHostAction::Refresh(workspace, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }

    fn remove(&mut self, request: RemoveSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Remove(_, completions))) = self
            .0
            .send(ControllerHostAction::Remove(request, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }

    fn sleep(&mut self, request: SleepSessionRequest, completions: Completions) {
        if let Err(mpsc::SendError(ControllerHostAction::Sleep(_, completions))) = self
            .0
            .send(ControllerHostAction::Sleep(request, completions))
        {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                "session command host is unavailable",
            ))));
        }
    }
}

impl BackendAgentPort for ControllerHost {
    fn launch_agent(&mut self, request: LaunchAgentRequest) {
        let _ = self.0.send(ControllerHostAction::LaunchAgent(request));
    }

    fn resume_agent(&mut self, request: ResumeAgentRequest) {
        let _ = self.0.send(ControllerHostAction::ResumeAgent(request));
    }

    fn reopen_agent(&mut self, request: ReopenAgentRequest) {
        let _ = self.0.send(ControllerHostAction::ReopenAgent(request));
    }

    fn open_terminal(&mut self, request: OpenTerminalRequest) {
        let _ = self.0.send(ControllerHostAction::OpenTerminal(request));
    }

    fn open_external_terminal(&mut self, target: Target) {
        let _ = self
            .0
            .send(ControllerHostAction::OpenExternalTerminal(target));
    }

    fn select_tab(&mut self, direction: crate::usecase::application::controller::TabDirection) {
        let _ = self.0.send(ControllerHostAction::SelectTab(direction));
    }
}

/// Complete production port set for one opened workspace.
pub struct ControllerBackendComposition {
    pub backend: DaemonBackend,
    pub session_commands: Box<dyn SessionCommandPort>,
    /// Resident session-inventory lane. It never shares the command port's
    /// connection, so a slow user-initiated create/remove and the background
    /// observation cannot block each other.
    pub session_refresh: Box<dyn SessionRefreshPort>,
    /// Resident terminal stream client. It stays with the live panes for the
    /// whole workspace and is never moved into a worker.
    pub agent_commands: Box<dyn AgentCommandPort>,
    /// Dedicated client shared by pane launch workers. It never shares the
    /// resident terminal stream connection.
    pub pane_launch_commands: Box<dyn PaneLaunchCommandPort>,
    /// Dedicated port moved into the off-thread restore job. It never shares
    /// the foreground terminal stream connection.
    pub restore_commands: Box<dyn AgentCommandPort>,
    /// Nonblocking, typed epochs from the dedicated restore connection. The
    /// controller drains this channel; it never probes daemon inventory from a
    /// frame tick.
    pub restore_connection: Box<dyn RestoreConnectionPort>,
    /// Dedicated port moved into the off-thread Garden observation job. It
    /// observes the *other* open projects' Agent inventory, so it never shares
    /// a connection with this workspace's own lanes.
    pub garden_inventory: Box<dyn GardenInventoryPort>,
    /// Dedicated serialized lane for daemon-owned `SupervisorRun` observation
    /// and human control. Serialization prevents an older snapshot from
    /// overtaking a control response in the UI.
    pub work_runs: Box<dyn WorkRunPort>,
    pub agent_tab_intents: Box<dyn AgentTabIntentPort>,
    pub external_terminal: Box<dyn ExternalTerminalPort>,
    pub metrics: Box<dyn MetricsPort>,
    pub browser: Box<dyn BrowserOpener>,
    /// Local worktree scan behind the inline create form's collision hint. It
    /// is a port so the frame loop's filesystem IO is countable in a test and
    /// stays out of the frame budget (#554).
    pub session_worktrees: Box<dyn SessionWorktreeScanPort>,
}

struct UnavailableWorkRunPort;

impl WorkRunPort for UnavailableWorkRunPort {
    fn snapshot(
        &mut self,
        _: WorkspaceId,
    ) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
        Err("Work Run progress is unavailable".to_owned())
    }

    fn control(
        &mut self,
        _: WorkspaceId,
        _: OperationId,
        _: usagi_core::domain::supervisor::SupervisorWorkspaceCommand,
    ) -> Result<WorkRunControlResult, WorkRunControlError> {
        Err(WorkRunControlError::Rejected(
            "Work Run action is unavailable; refresh and try again".to_owned(),
        ))
    }
}

struct UnavailableGardenInventoryPort;

impl GardenInventoryPort for UnavailableGardenInventoryPort {
    fn inventory(&mut self, _: WorkspaceId) -> Result<AgentWorkspaceObservation, String> {
        Err("Agent inventory is unavailable".to_owned())
    }
}

struct UnavailableRestoreConnectionPort;

impl RestoreConnectionPort for UnavailableRestoreConnectionPort {
    fn take_reconnected_epoch(&mut self) -> Option<u64> {
        None
    }
}

/// Single factory used by direct launch and every screen-graph workspace entry.
pub trait ControllerBackendFactory {
    /// Process-level motion preference resolved by the composition root. Fakes
    /// keep full motion unless a test opts in explicitly.
    fn garden_reduced_motion(&self) -> bool {
        false
    }

    fn create(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition;
}

struct UnavailableBackendPort;

struct UnavailableExternalTerminalPort;

impl ExternalTerminalPort for UnavailableExternalTerminalPort {
    fn open(&mut self, _: &Path) -> Result<(), String> {
        Err("external terminal launch is unavailable".to_owned())
    }
}

fn unavailable_completion(completions: &Completions, message: &str) {
    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
        message,
    ))));
}

impl BackendTargetStorePort for UnavailableBackendPort {
    fn load_notes(&mut self, _: Target, completions: Completions) {
        unavailable_completion(&completions, "notes are unavailable");
    }
    fn save_notes(
        &mut self,
        _: Target,
        _: usagi_core::domain::note::Scratchpad,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "notes are unavailable");
    }
    fn load_environment(&mut self, _: EnvScope, completions: Completions) {
        unavailable_completion(&completions, "environment is unavailable");
    }
    fn save_environment(
        &mut self,
        _: EnvScope,
        _: Vec<EnvironmentEntry>,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "environment is unavailable");
    }
}

impl BackendWorkspaceCommandPort for UnavailableBackendPort {
    fn execute(
        &mut self,
        _: WorkspaceId,
        _: crate::usecase::overview::Command,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "workspace command is unavailable");
    }
}

impl BackendDecisionPort for UnavailableBackendPort {
    fn refresh(&mut self, _: WorkspaceId, completions: Completions) {
        unavailable_completion(&completions, "user decisions are unavailable");
    }
    fn resolve(
        &mut self,
        _: WorkspaceId,
        _: UserDecisionId,
        _: UserDecisionAnswer,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "user decisions are unavailable");
    }
}

impl BackendOverlayPort for UnavailableBackendPort {
    fn load_pull_requests(&mut self, _: Target, completions: Completions) {
        unavailable_completion(&completions, "Pull Request data is unavailable");
    }
    fn load_preview(
        &mut self,
        _: Target,
        _: RequestId,
        _: Option<String>,
        _: PreviewFileFilter,
        completions: Completions,
    ) {
        unavailable_completion(&completions, "preview is unavailable");
    }
    fn open_pull_request(&mut self, _: String, completions: Completions) {
        unavailable_completion(&completions, "browser opening is unavailable");
    }
}

/// 起動バナーを `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_banner(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{}", info.describe())
}

/// 対話ループが終了する理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// ユーザーが終了した（`q` / Ctrl-C、または起点画面で Esc）。プロセスを終える。
    Quit,
    /// 利用者が workspace を離れて Welcome へ戻ることを選んだ。プロセスは終わらない。
    ///
    /// 返すのは workspace 単体の runner だけである。screen graph はこの理由を自分の
    /// ループ内で `Screen::Welcome` へ解決するため、[`run_screen_graph_with_backend`]
    /// がこれを返すことはない（#556）。
    Welcome,
}

/// 対話ループの開始画面。合成ルートが `usagi`（Welcome）か `usagi config`（Config）かで選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// トップメニュー（Welcome）から始める。
    Welcome,
    /// 設定画面（Config）から始める。
    Config,
}

/// いま表示している画面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Welcome,
    Open,
    New,
    Config,
}

/// welcome 画面のキー処理結果。
enum WelcomeStep {
    Stay,
    Quit,
    OpenList,
    /// Recent の単体 workspace を開く。
    OpenRecent(usize),
    /// New（新規 workspace 作成フォーム）へ進む。
    NewForm,
    /// Config（設定画面）へ進む。
    ConfigScreen,
}

/// Config 画面でキー `key` を処理した結果の遷移。
enum ConfigStep {
    /// 同じ画面に留まる。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
    /// A save has begun (loading). The screen graph animates the Save button,
    /// writes, then on success holds the `done` frame before returning home; a
    /// failed write stays on Config with an error for retry.
    Save,
    /// A validated environment write should be persisted by the caller.
    SaveEnvironment,
}

/// Draw one complete highlight sweep across the pending Save button. Settings
/// writes are normally too quick for an intermediate state to be perceptible,
/// so the short, fixed sweep makes the transition visible before persistence.
fn play_config_save_wave(
    term: &mut dyn Terminal,
    form: &mut Config,
    base: Option<&[String]>,
) -> io::Result<()> {
    for frame in 0..config::SAVE_WAVE_FRAMES {
        let (height, width) = term.size()?;
        let lines = match base {
            Some(base) => config::render_over(height, width, base, form),
            None => config::render(height, width, form),
        };
        term.draw(&lines)?;
        if frame + 1 < config::SAVE_WAVE_FRAMES {
            term.wait(config::SAVE_WAVE_TICK)?;
            form.advance_save_animation();
        }
    }
    Ok(())
}

fn save_config_responsive(
    term: &mut dyn Terminal,
    form: &mut Config,
    settings: &mut dyn SettingsPort,
    base: Option<&[String]>,
) -> io::Result<bool> {
    if !settings.background_operations() {
        play_config_save_wave(term, form, base)?;
        return Ok(form.commit_save(settings));
    }
    let Some((scope, draft)) = form.save_request() else {
        return Ok(false);
    };
    let result = if let Some(base) = base {
        run_config_save_loading(term, form, base, || settings.save(scope, &draft))
    } else {
        run_workspace_loading(term, "Saving settings…", false, || {
            settings.save(scope, &draft)
        })
    };
    Ok(form.finish_save(draft, result))
}

/// Persist a workspace Config draft without replacing its Home-owned modal.
///
/// The generic workspace loading surface clears the complete frame. Using it
/// here made Config disappear during the write, then reappear for `done` before
/// closing. Keep painting the pending Config form over the same Home snapshot
/// so one modal remains visible throughout the save lifecycle.
fn run_config_save_loading<T: Send>(
    term: &mut dyn Terminal,
    form: &mut Config,
    base: &[String],
    operation: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    run_workspace_loading_with(
        term,
        "Saving settings…",
        false,
        false,
        operation,
        |height, width, _frame, _status, _show_progress| {
            let lines = config::render_over(height, width, base, form);
            form.advance_save_animation();
            lines
        },
    )
}

fn save_environment_responsive(
    term: &mut dyn Terminal,
    form: &mut Config,
    settings: &mut dyn SettingsPort,
) -> bool {
    let Some((scope, bindings)) = form.environment_save_request() else {
        return false;
    };
    let result = if settings.background_operations() {
        run_workspace_loading(term, "Saving environment…", false, || {
            settings.save_environment(scope, &bindings)
        })
    } else {
        settings.save_environment(scope, &bindings)
    };
    form.finish_environment_save(bindings, result)
}

/// Workspace Config is a Home-owned modal and therefore cannot request that the
/// enclosing TUI exit. Quit chords are projected to [`Self::Stay`] at the modal
/// input boundary.
enum WorkspaceConfigStep {
    Stay,
    Back,
    Save,
    SaveEnvironment,
}

/// New 画面でキー `key` を処理した結果の遷移。
enum NewStep {
    /// 同じ画面に留まる（フォーム編集を続ける）。
    Stay,
    /// 終了する。
    Quit,
    /// welcome へ戻る。
    Back,
    /// Ask the composition root to enumerate one directory's immediate children.
    CompleteDirectory(DirectoryCompletion),
    /// 検証済みの入力で workspace 作成を実行する。screen graph が backend を 1 回呼ぶ。
    Create(NewRequest),
}

/// One create admitted by the entry loop. `cancelled` is a navigation fence:
/// the worker may still finish, but its completion can no longer open a
/// workspace after the user leaves New.
struct PendingWorkspaceCreate {
    token: WorkspaceCreateToken,
    request: NewRequest,
    cancelled: bool,
}

/// A selected workspace path that disappeared before it could be opened.
/// The prompt owns only entry-screen state; the loader revalidates the paths
/// before any registry mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MissingWorkspacePrompt {
    paths: Vec<PathBuf>,
    confirmation: modal::ConfirmationModal,
}

impl MissingWorkspacePrompt {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
            confirmation: modal::ConfirmationModal::new(),
        }
    }
}

/// Open 画面のキー処理結果。
enum OpenStep {
    Stay,
    Quit,
    Back,
    Choose(Vec<PathBuf>),
    ConfirmCleanup,
    ConfirmUnregister(PathBuf),
}

/// Workspace 画面のキー処理結果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkspaceStep {
    /// TUI を終了する。
    Quit,
    /// workspace を離れて Welcome へ戻る。呼び出し側がこの workspace のために
    /// 確立した資源を落としたあと、entry 画面を描き直す（#556）。
    Back,
    /// A target snapshot was prepared while the current composition was still
    /// resident. Returning drops that composition; the deck loop then commits
    /// the target and creates its replacement.
    Activate(Box<WorkspaceSnapshot>),
}

impl WorkspaceStep {
    /// workspace ループの停止理由を TUI 全体の終了理由へ投影する。workspace を
    /// 直接開いた入口（`usagi open <path>`）は Welcome を持たないため、合成ルートが
    /// [`Exit::Welcome`] を受けて screen graph へ入り直す。
    fn exit(self) -> Exit {
        match self {
            Self::Quit => Exit::Quit,
            Self::Back | Self::Activate(_) => Exit::Welcome,
        }
    }
}

struct WorkspaceConfigContext<'a> {
    settings: &'a mut dyn SettingsPort,
    available_models: AvailableAgentModels,
}

/// Effective defaults needed when entering one workspace.
///
/// Agent availability is observed by the composition root (the shell owns the
/// PATH probe); model and branch defaults come from effective settings. The
/// fallback fits callers without resolved settings: every CLI offered, `codex`
/// selected, and the current checkout used for new sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceEntryPolicy {
    available_models: AvailableAgentModels,
    default_model: usagi_core::domain::settings::DefaultModel,
    default_branch: Option<String>,
    work_mode: usagi_core::domain::settings::WorkMode,
    icon_mode: usagi_core::domain::settings::IconMode,
}

impl Default for WorkspaceEntryPolicy {
    fn default() -> Self {
        Self {
            available_models: AvailableAgentModels::all(),
            default_model: usagi_core::domain::settings::DefaultModel::default(),
            default_branch: None,
            work_mode: usagi_core::domain::settings::WorkMode::default(),
            icon_mode: usagi_core::domain::settings::IconMode::default(),
        }
    }
}

struct UnavailableSessionCommandPort;

impl SessionCommandPort for UnavailableSessionCommandPort {
    fn execute(
        &self,
        _workspace: &usagi_core::domain::workspace::Workspace,
        _selected: Option<&usagi_core::domain::session::SessionRecord>,
        _command: SessionCommand,
    ) -> Result<SessionCommandResult, String> {
        Err("session commands are unavailable".to_owned())
    }
}

/// 既定では Agent launch を接続しない port。
///
/// daemon-backed Agent factory を注入しない screen-graph 経路（`run_with_settings`）で
/// controller ループを駆動するためのフォールバック。launch はインラインの失敗になり、
/// ローカルでプロセスを起動しない。
struct UnavailableAgentCommandPort;
impl AgentCommandPort for UnavailableAgentCommandPort {
    fn launch(
        &mut self,
        _operation: OperationId,
        _workspace: WorkspaceId,
        _session: Option<SessionId>,
        _profile: Option<AgentProfileId>,
    ) -> Result<AgentPaneAdmission, String> {
        Err("Agent launch is unavailable.".to_owned())
    }
}

struct UnavailableAgentTabIntentPort;

impl AgentTabIntentPort for UnavailableAgentTabIntentPort {
    fn load(&mut self, workspace: WorkspaceId) -> Result<AgentTabIntent, AgentTabIntentError> {
        Ok(AgentTabIntent::empty(workspace))
    }

    fn mutate(
        &mut self,
        workspace: WorkspaceId,
        _expected_revision: u64,
        mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentPortCommit, AgentTabIntentError> {
        let mut intent = AgentTabIntent::empty(workspace);
        let projection = intent.apply(mutation);
        Ok(AgentTabIntentPortCommit {
            intent,
            projection,
            mutation_applied: true,
            cas_conflict: false,
        })
    }
}

/// Decision fallback for the screen-graph compatibility path. Production
/// composition injects its daemon-backed counterpart.
#[cfg(test)]
struct UnavailableDecisionCommandPort;
#[cfg(test)]
impl DecisionCommandPort for UnavailableDecisionCommandPort {
    fn refresh(&mut self, _workspace: WorkspaceId) -> BackendEvent {
        BackendEvent::Notice(Notice::new("User decisions are unavailable."))
    }

    fn resolve(
        &mut self,
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        _answer: UserDecisionAnswer,
    ) -> BackendEvent {
        BackendEvent::DecisionError {
            workspace,
            decision_id,
            error: SafeError {
                message: SafeMessage::new("User decisions are unavailable."),
                error_id: "decision-unavailable".to_owned(),
            },
        }
    }
}

/// Environment fallback for the screen-graph compatibility path and embedders
/// that inject no store. Production composition injects its state-backed
/// counterpart; this keeps the editor safe (it stays open, showing the error)
/// rather than silently discarding a load or save.
#[cfg(test)]
struct UnavailableEnvironmentStore;
#[cfg(test)]
impl EnvironmentStorePort for UnavailableEnvironmentStore {
    fn load(&mut self, scope: EnvScope) -> BackendEvent {
        BackendEvent::EnvironmentError {
            scope,
            error: unavailable_environment_error(),
        }
    }

    fn save(&mut self, scope: EnvScope, _entries: Vec<EnvironmentEntry>) -> BackendEvent {
        BackendEvent::EnvironmentError {
            scope,
            error: unavailable_environment_error(),
        }
    }
}

#[cfg(test)]
fn unavailable_environment_error() -> SafeError {
    SafeError {
        message: SafeMessage::new("Environment is unavailable."),
        error_id: "environment-unavailable".to_owned(),
    }
}

/// PR snapshot fallback for entry points that do not inject the daemon PR port
/// (the Welcome/Open/Recent screen graph). The PR overlay shows a safe notice.
#[cfg(test)]
struct UnavailablePrSnapshotPort;
#[cfg(test)]
impl PrSnapshotPort for UnavailablePrSnapshotPort {
    fn snapshot(
        &mut self,
        _session: SessionId,
    ) -> Result<usagi_core::infrastructure::client::PrSnapshot, String> {
        Err("Pull Request data is unavailable.".to_owned())
    }
}

/// Browser-open fallback for entry points that do not inject a platform opener.
struct UnavailableBrowserOpener;
impl BrowserOpener for UnavailableBrowserOpener {
    fn open(&mut self, _url: &str) -> Result<(), String> {
        Err("Browser opening is unavailable on this platform.".to_owned())
    }
}

/// The lane an embedder that injects no daemon-backed worker gets: it observes
/// nothing, so Home keeps the snapshot it opened with.
struct UnavailableSessionRefreshPort;

impl SessionRefreshPort for UnavailableSessionRefreshPort {
    fn wake(&mut self) {}

    fn take(&mut self) -> Option<Result<SessionCommandResult, String>> {
        None
    }
}

/// 既定では session command を接続しない factory。
///
/// daemon-backed port を注入しない embedder / テスト経路で使う。
struct UnavailableSessionCommandPortFactory;

impl SessionCommandPortFactory for UnavailableSessionCommandPortFactory {
    fn create(&mut self) -> Box<dyn SessionCommandPort> {
        Box::new(UnavailableSessionCommandPort)
    }
}

/// daemon IO transport that the controller runtime keeps alongside its
/// [`WorkspaceRuntime`]: the session-create worker, the daemon-authoritative
/// session cache ([`WorkspaceView`]), pane launch workers, and live terminal
/// streams. Daemon metrics / git diffs are refluxed separately through
/// [`MetricsBackend`]. Home row state, input, and rendering belong to
/// the controller (`AppState`/`render_home`), not here.
struct WorkspaceIoRuntime {
    workspace: WorkspaceView,
    /// Shared daemon boundary. Admission allows one lifecycle worker at a time;
    /// snapshot revisions additionally fence stale authoritative observations.
    session_commands: std::sync::Arc<dyn SessionCommandPort>,
    last_session_revision: u64,
    /// Non-sensitive interrupted/resume state received from the daemon.
    agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
    /// Latest coherent workspace-wide Agent inventory received by the restore
    /// lane. Kept as draw material for the read-only daemon status modal.
    agent_inventory: Option<AgentInventory>,
    material_revision: u64,
    session_completions: Receiver<SessionCommandCompletion>,
    session_completion_sender: Sender<SessionCommandCompletion>,
    /// Monotonic fence for the one admitted session command. A delayed or
    /// synthetic completion can never return its port into a newer command.
    next_session_command: u64,
    active_session_command: Option<u64>,
    /// Session displayed as a removal skeleton until its daemon command returns.
    removing_session: Option<SessionId>,
    /// An in-flight create's controller token and the name drawn in its sidebar
    /// skeleton (`document/03-tui.md`). Its completion can reflux a failure to
    /// the reducer as an [`OperationResult`]. `Some` only while a create worker
    /// owns the admission slot, so the skeleton clears when its result lands.
    creating_session: Option<PendingCreate>,
    agent: Option<AgentContext>,
    external_terminal: Box<dyn ExternalTerminalPort>,
    /// Shared launch client. Workers borrow it through the `Arc`, so the
    /// resident stream port stays with the live panes and a worker that hangs,
    /// panics, or loses its completion cannot take the capability away.
    pane_launch_commands: std::sync::Arc<dyn PaneLaunchCommandPort>,
    /// Launches admitted and rendered as pending, oldest first. Bounded by
    /// [`PANE_LAUNCH_QUEUE_LIMIT`]; a request beyond the bound completes
    /// immediately as Busy instead of joining the queue.
    pane_launches: Vec<PaneLaunch>,
    pane_completions: Receiver<PaneLaunchCompletion>,
    pane_completion_sender: Sender<PaneLaunchCompletion>,
    /// Monotonic fence for the one admitted launch worker. A late, duplicate, or
    /// unadmitted completion can never free a newer worker's slot.
    next_pane_launch: u64,
    active_pane_launch: Option<u64>,
    /// Live coordinators for terminals visible in the current frame. This is
    /// normally the selected foreground terminal; Director additionally keeps
    /// the dimmed managed-session preview attached. Hidden and unselected tabs
    /// retain only their stable pane identity.
    terminals: Vec<TerminalSession>,
    /// Recently detached coordinators, oldest first. Keeping the coordinator
    /// preserves its connection-local input ledger and unresolved input fence.
    detached_terminals: VecDeque<TerminalSession>,
    /// Generic terminals the user logically closed in this workspace UI. The
    /// daemon keeps their PTYs alive, so a later inventory must not immediately
    /// recreate their tabs. A fresh workspace UI starts empty and restores them
    /// again; an explicit `terminal open` also removes the exact fence.
    closed_generic_terminals: BTreeSet<TerminalRef>,
    terminal_reconnected: bool,
    terminal_size: (usize, usize),
    agent_tab_intent: Option<AgentTabIntentContext>,
    /// A successful durable Reopen requests one fresh coherent daemon
    /// observation. It never projects from an inventory cached before a later
    /// pane admission.
    agent_observation_requested: bool,
    /// A successful Agent launch/resume or terminal exit changes daemon
    /// inventory. Unlike a display-only observation request, this must schedule
    /// one follow-up when an older restore snapshot is already in flight.
    agent_inventory_change_observation_requested: bool,
}

struct AgentTabIntentContext {
    workspace: WorkspaceId,
    allowed_sessions: BTreeSet<SessionId>,
    state: AgentTabIntent,
    port: Box<dyn AgentTabIntentPort>,
    /// Exact identities that were actually admitted to a runtime projection.
    /// Kept across a stale CAS observation so closing a still-visible O can
    /// dismiss its continuation while a fresh observation for R is in flight.
    visible_agents: Vec<(TerminalRef, AgentContinuationRef)>,
    load_error: Option<AgentTabIntentError>,
}

struct AgentTabObservation {
    projection: AgentTabProjection,
    cas_accepted: bool,
}

struct RestoreCompletion {
    port: Box<dyn AgentCommandPort>,
    dispatched_interaction: u64,
    dispatched_registry_revision: u64,
    dispatched_allowed_sessions: BTreeSet<SessionId>,
    terminals: Result<Vec<TerminalInventoryEntry>, TerminalError>,
    agents: Result<AgentInventory, String>,
    observation_coherent: bool,
}

struct RestoreApply {
    port: Box<dyn AgentCommandPort>,
    outcome: RestoreJobOutcome,
}

/// Steady cadence of the Garden's cross-project observation while the screen
/// saver is up. It bounds how stale another project's rabbits can be: cadence
/// plus one round of requests.
const GARDEN_OBSERVATION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1_000);

/// Cadence after a round that observed nothing (no daemon, refused workspace).
/// A Garden left open in front of a dead daemon must not retry every second.
const GARDEN_OBSERVATION_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5_000);

/// Most open projects observed in one round. Beyond this the extra tabs keep
/// their read-only plots rather than letting one round's request count follow an
/// unbounded tab list.
const MAX_OBSERVED_PROJECTS: usize = 16;

/// One completed round of the Garden's cross-project observation.
struct GardenObservationCompletion {
    port: Box<dyn GardenInventoryPort>,
    /// Inventories the daemon answered, each already checked to be the
    /// workspace it was asked for.
    inventories: Vec<AgentWorkspaceObservation>,
}

const WORK_RUN_OBSERVATION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2_000);
const WORK_RUN_OBSERVATION_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5_000);
enum WorkRunLaneCompletion {
    Observation {
        port: Box<dyn WorkRunPort>,
        snapshot: Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String>,
    },
    Control {
        port: Box<dyn WorkRunPort>,
        operation_id: OperationId,
        result: Box<Result<WorkRunControlResult, WorkRunControlError>>,
    },
}

/// Observe the other open projects' Agent inventory off the frame thread.
///
/// A workspace whose daemon does not answer, or answers with another
/// workspace's inventory, is skipped: the Garden keeps that project's read-only
/// plot rather than drawing a foreign project's rabbits in it.
fn spawn_garden_observation_job(
    mut port: Box<dyn GardenInventoryPort>,
    workspaces: Vec<WorkspaceId>,
    sender: Sender<GardenObservationCompletion>,
) {
    std::thread::spawn(move || {
        let mut inventories = Vec::new();
        for workspace in workspaces.into_iter().take(MAX_OBSERVED_PROJECTS) {
            if let Ok(inventory) = port.inventory(workspace)
                && inventory.inventory.workspace_id == workspace
            {
                inventories.push(inventory);
            }
        }
        let _ = sender.send(GardenObservationCompletion { port, inventories });
    });
}

fn spawn_work_run_observation_job(
    mut port: Box<dyn WorkRunPort>,
    workspace: WorkspaceId,
    sender: Sender<WorkRunLaneCompletion>,
) {
    std::thread::spawn(move || {
        let snapshot =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| port.snapshot(workspace)))
                .unwrap_or_else(|_| Err("Work Run progress is temporarily unavailable".to_owned()))
                .and_then(|snapshot| validate_work_run_snapshot(snapshot, workspace));
        let _ = sender.send(WorkRunLaneCompletion::Observation { port, snapshot });
    });
}

fn spawn_work_run_control_job(
    mut port: Box<dyn WorkRunPort>,
    workspace: WorkspaceId,
    request: WorkRunControlRequest,
    sender: Sender<WorkRunLaneCompletion>,
) {
    std::thread::spawn(move || {
        let operation_id = request.operation_id;
        let expected = request.command.supervisor_run_id();
        let expected_revision = request.observed_state_revision;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            port.control(workspace, operation_id, request.command)
        }))
        .unwrap_or_else(|_| {
            Err(WorkRunControlError::Unconfirmed(
                WORK_RUN_ACTION_UNCONFIRMED.to_owned(),
            ))
        })
        .and_then(|result| {
            let valid = match &result {
                WorkRunControlResult::Updated(run) => {
                    run.supervisor_run_id == expected && run.provenance.is_empty()
                }
                WorkRunControlResult::Deleted(deletion) => {
                    deletion.supervisor_run_id == expected
                        && deletion.state_revision == expected_revision
                }
            };
            valid.then_some(result).ok_or_else(|| {
                WorkRunControlError::Unconfirmed(
                    "daemon returned an invalid Work Run result".to_owned(),
                )
            })
        });
        let _ = sender.send(WorkRunLaneCompletion::Control {
            port,
            operation_id,
            result: Box::new(result),
        });
    });
}

fn validate_work_run_snapshot(
    snapshot: usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot,
    workspace: WorkspaceId,
) -> Result<usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot, String> {
    let unique = snapshot
        .runs
        .iter()
        .map(|run| run.supervisor_run_id)
        .collect::<BTreeSet<_>>()
        .len()
        == snapshot.runs.len();
    if snapshot.workspace_id != workspace {
        Err("daemon returned another workspace's Work Runs".to_owned())
    } else if snapshot.runs.len() > MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS
        || !unique
        || snapshot.runs.iter().any(|run| !run.provenance.is_empty())
    {
        Err("daemon returned invalid Work Run progress".to_owned())
    } else {
        Ok(snapshot)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreJobOutcome {
    Applied,
    FenceRejected,
    TransportFailed,
    IntentFailed(AgentTabIntentError),
}

/// Background exits applied per frame. The observation lane queues them, so one
/// frame's tab-closing work stays bounded however many background tabs exited at
/// once; the rest are applied by the next frames.
const MAX_BACKGROUND_EXITS_PER_FRAME: usize = 8;
const DETACHED_TERMINAL_LIMIT: usize = 8;
/// The process-level project tab bar permanently owns the first terminal row.
const PROJECT_BAR_ROWS: usize = 1;
const WORKSPACE_SWITCH_LOADING_GRACE: std::time::Duration = std::time::Duration::from_millis(80);

const RESTORE_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(250);
const RESTORE_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(4);

/// Controller-owned admission and backoff for the dedicated restore client.
/// Frame ticks only consult this clock; they never imply a reconnect or issue an
/// inventory RPC by themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RestoreRetryState {
    in_flight: bool,
    followup: RestoreFollowup,
    failures: u32,
    next_retry_at: Option<std::time::Duration>,
    notice_emitted: bool,
    last_reconnect_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreFollowup {
    None,
    ChangedObservation,
    Reconnected,
}

impl RestoreRetryState {
    fn new() -> Self {
        Self {
            in_flight: false,
            followup: RestoreFollowup::None,
            failures: 0,
            next_retry_at: Some(std::time::Duration::ZERO),
            notice_emitted: false,
            last_reconnect_epoch: 0,
        }
    }

    fn begin_if_due(&mut self, now: std::time::Duration) -> bool {
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
    fn request_observation(&mut self, now: std::time::Duration) {
        if !self.in_flight && self.next_retry_at.is_none() {
            self.next_retry_at = Some(now);
        }
    }

    /// Request a snapshot after daemon inventory changed. A snapshot already in
    /// flight may predate that change, so remember one coalesced follow-up.
    fn request_changed_observation(&mut self, now: std::time::Duration) {
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
    fn complete(&mut self, now: std::time::Duration, outcome: RestoreJobOutcome) -> bool {
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
    fn reconnected(&mut self, epoch: u64, now: std::time::Duration) {
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

/// A create request in flight: the controller token used to reflux a failure and
/// the typed name shown in the sidebar's loading skeleton until the daemon's
/// `session.created` row replaces it.
struct PendingCreate {
    name: String,
}

struct AgentContext {
    workspace: WorkspaceId,
    sessions: Vec<SessionId>,
    /// Resident terminal stream port. Attach, poll, input, resize, and detach
    /// keep using it for the whole workspace: no launch, resume, or restore
    /// worker ever takes it, so a slow daemon request cannot stop pane IO.
    port: Box<dyn AgentCommandPort>,
}

struct SessionCommandCompletion {
    command_id: u64,
    result: Result<SessionCommandResult, String>,
    completion: SessionBackendCompletion,
}

enum SessionBackendCompletion {
    Create {
        token: PendingToken,
        before: Vec<SessionId>,
        completions: Completions,
    },
    Remove {
        session: SessionId,
        before: Vec<SessionId>,
        completions: Completions,
    },
    Sleep {
        before: Vec<SessionId>,
        completions: Completions,
    },
}

/// Completion of one non-blocking Agent / terminal launch.
///
/// No port travels in the message: the launch client is shared and the resident
/// stream port never left the UI, so a completion carries only the fenced
/// identity of the operation it finishes.
struct PaneLaunchCompletion {
    /// The admitted worker's fence, or [`PANE_LAUNCH_UNADMITTED`] for a
    /// completion no worker produced (an admission refusal).
    launch_id: u64,
    outcome: PaneLaunchOutcome,
}

#[derive(Clone)]
enum PaneLaunchOutcome {
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

/// Pending pane launches admitted beyond the one worker that owns the launch
/// client. The bound keeps a burst of activations from growing an unbounded queue
/// of pending tabs; a request past it completes as Busy instead of joining.
const PANE_LAUNCH_QUEUE_LIMIT: usize = 8;

/// Launch fence reserved for a completion no worker produced (an admission
/// refusal). Worker fences start at [`PANE_LAUNCH_FIRST`], so an unadmitted or
/// late completion can never free an admitted worker's slot.
const PANE_LAUNCH_UNADMITTED: u64 = 0;
const PANE_LAUNCH_FIRST: u64 = 1;

/// Safe feedback for a launch refused by bounded admission. The daemon never saw
/// the request, so retrying it is safe.
const PANE_LAUNCH_BUSY: &str = "too many pane launches are pending; try again";

/// Safe feedback for a worker that died inside the launch client. The request's
/// daemon effect is unknown, so the pane fails instead of being retried silently.
const PANE_LAUNCH_WORKER_FAILED: &str = "pane launch failed; check the daemon";

/// A pane has already been rendered as pending before this work is run.
enum PaneLaunch {
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
    fn identity(&self) -> PaneLaunchIdentity {
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
enum PaneLaunchIdentity {
    Agent(OperationId),
    Terminal(OperationId),
    ResumeExact(OperationId, AgentContinuationRef),
}

impl PaneLaunchIdentity {
    fn operation(self) -> OperationId {
        match self {
            Self::Agent(operation)
            | Self::Terminal(operation)
            | Self::ResumeExact(operation, _) => operation,
        }
    }

    /// The one safe-failure completion this pane gets when its request never
    /// reached the daemon (admission refusal) or its worker died.
    fn failed(self, message: &str) -> PaneLaunchOutcome {
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

impl WorkspaceIoRuntime {
    fn new(workspace: WorkspaceView, session_commands: Box<dyn SessionCommandPort>) -> Self {
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

    fn set_terminal_size(&mut self, height: usize, width: usize) {
        self.terminal_size = (height, width);
    }

    /// Bind the resident terminal stream port of one workspace. Pane launches
    /// use their own client ([`Self::with_pane_launch_port`]).
    fn with_agent_context(
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
    fn with_pane_launch_port(mut self, port: Box<dyn PaneLaunchCommandPort>) -> Self {
        self.pane_launch_commands = std::sync::Arc::from(port);
        self
    }

    fn with_agent_tab_intent(
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

    fn take_agent_tab_intent_load_error(&mut self) -> Option<AgentTabIntentError> {
        self.agent_tab_intent
            .as_mut()
            .and_then(|context| context.load_error.take())
    }

    fn with_agent_resumes(
        mut self,
        agent_resumes: BTreeMap<SessionId, ProviderResumeProjection>,
    ) -> Self {
        self.agent_resumes = agent_resumes;
        self
    }

    fn with_external_terminal(mut self, port: Box<dyn ExternalTerminalPort>) -> Self {
        self.external_terminal = port;
        self
    }

    /// Attach to a freshly launched daemon terminal and start streaming it.
    ///
    /// A failed attach still records the session so its safe feedback renders;
    /// it never spawns a local process.
    fn start_terminal_session(&mut self, terminal: TerminalRef, geometry: Geometry) {
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
    fn sync_foreground_terminal(&mut self, focused: Option<&TerminalRef>, geometry: Geometry) {
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
    fn sync_visible_terminals(&mut self, visible: &[(TerminalRef, Geometry)]) {
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
    fn list_open_terminals(&mut self) -> Result<Vec<TerminalInventoryEntry>, ()> {
        match self.agent.as_mut() {
            Some(agent) => agent.port.list_terminals().map_err(|_| ()),
            None => Ok(Vec::new()),
        }
    }

    #[cfg(test)]
    fn resize_terminals(&mut self, geometry: Geometry) {
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
    fn send_terminal_bytes(&mut self, terminal: &TerminalRef, bytes: &[u8]) -> Result<(), String> {
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

    fn clear_terminal_for_user(&mut self, terminal: &TerminalRef) -> bool {
        self.terminals
            .iter_mut()
            .find(|session| session.terminal().fences(terminal))
            .is_some_and(TerminalSession::clear_for_user)
    }

    /// Poll every attached terminal once and return the refs of those the daemon
    /// reports as exited. Polling all of them (not just the focused pane) is what
    /// lets a background tab whose shell ran `exit` be detected and closed.
    fn poll_all_terminals(&mut self) -> Vec<TerminalRef> {
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

    fn take_terminal_row_motions(&mut self) -> Vec<(TerminalRef, Vec<RetainedRowMotion>)> {
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
    fn sync_background_terminals(&mut self, background: &[TerminalRef]) -> Vec<TerminalRef> {
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

    fn take_terminal_reconnected(&mut self) -> bool {
        std::mem::take(&mut self.terminal_reconnected)
    }

    /// Release a terminal's client subscription and retain its coordinator in a
    /// bounded LRU. The daemon keeps the process and connection-local input
    /// ledger; a later attach therefore preserves ordering and unresolved input.
    fn close_terminal(&mut self, terminal: &TerminalRef) {
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
    fn close_generic_terminal(&mut self, terminal: &TerminalRef) {
        self.closed_generic_terminals.insert(terminal.clone());
        self.close_terminal(terminal);
    }

    /// Whether a launch completion points back to a shell whose exit has not
    /// reached coherent inventory yet. Reusing it would resurrect the exact
    /// scrollback the user just closed.
    fn generic_terminal_is_closing(&self, terminal: &TerminalRef) -> bool {
        self.closed_generic_terminals
            .iter()
            .any(|closed| closed.fences(terminal))
    }

    /// A coherent inventory proves which process-local close fences can still
    /// matter. Exited terminals no longer need suppression, keeping this set
    /// bounded by the daemon's live generic inventory.
    fn reconcile_closed_generic_terminals(&mut self, terminals: &[TerminalInventoryEntry]) {
        self.closed_generic_terminals.retain(|closed| {
            terminals.iter().any(|entry| {
                entry.live && entry.kind == TerminalKind::Terminal && entry.terminal.fences(closed)
            })
        });
    }

    /// Return the authoritative inventory with only this UI's logically closed
    /// generic terminals hidden. Agent rows and every other terminal row stay
    /// unchanged for reconciliation.
    fn restorable_terminal_inventory(
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

    fn agent_continuation_for(&self, terminal: &TerminalRef) -> Option<AgentContinuationRef> {
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

    fn observe_agent_tabs(
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

    fn mutate_agent_intent(
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

    fn request_agent_observation(&mut self) {
        self.agent_observation_requested = true;
    }

    fn take_agent_observation_request(&mut self) -> bool {
        std::mem::take(&mut self.agent_observation_requested)
    }

    fn request_agent_inventory_change_observation(&mut self) {
        self.agent_inventory_change_observation_requested = true;
    }

    fn take_agent_inventory_change_observation_request(&mut self) -> bool {
        std::mem::take(&mut self.agent_inventory_change_observation_requested)
    }

    fn agent_inventory(&self) -> Option<&AgentInventory> {
        self.agent_inventory.as_ref()
    }

    /// Opening the daemon modal starts from an explicit loading projection and
    /// asks the existing coalesced restore lane for one fresh coherent snapshot.
    fn refresh_agent_inventory(&mut self) {
        self.agent_inventory = None;
        self.material_revision = self.material_revision.saturating_add(1);
        self.request_agent_observation();
    }

    /// The saved Agent slot order of the whole workspace, flattened across
    /// targets. It gives a restored interrupted tab the position the user last
    /// saw it in (#506 slots keyed by lineage).
    fn agent_slot_order(&self) -> Vec<AgentContinuationRef> {
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
    fn agent_dismissed(&self) -> BTreeSet<AgentContinuationRef> {
        self.agent_tab_intent
            .as_ref()
            .map_or_else(BTreeSet::new, |context| context.state.dismissed.clone())
    }

    fn has_agent_intent_for(&self, session_id: Option<SessionId>) -> bool {
        self.agent_tab_intent.as_ref().is_some_and(|context| {
            context
                .state
                .targets
                .iter()
                .find(|target| target.session_id == session_id)
                .is_some_and(|target| !target.tabs.is_empty())
        })
    }

    fn set_allowed_agent_sessions(&mut self, sessions: impl IntoIterator<Item = SessionId>) {
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
    fn terminal_rows(
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

    fn terminal_row_extent(
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

    fn terminal_projection_key(&self, terminal: &TerminalRef) -> Option<u64> {
        self.terminals
            .iter()
            .chain(self.detached_terminals.iter())
            .find(|session| session.terminal().fences(terminal))
            .map(TerminalSession::projection_key)
    }

    fn terminal_input_modes(&self, terminal: &TerminalRef) -> Option<TerminalInputModes> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(TerminalSession::input_modes)
    }

    /// Snapshot the retained rows of a background terminal without changing its
    /// attachment or scroll controls. A workspace drawer keeps the managed pane
    /// underneath it attached, so this projection advances as its live stream is
    /// drained even while the overlay covers it.
    fn retained_terminal_view(
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

    fn terminal_row_window(
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
    fn terminal_cells(&self, terminal: &TerminalRef) -> Option<Vec<String>> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(TerminalSession::cells)
    }

    fn begin_terminal_selection(
        &self,
        terminal: &TerminalRef,
        anchor: TerminalPoint,
    ) -> Option<TerminalSelection> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .map(|session| session.begin_selection(anchor))
    }

    fn terminal_error(&self, terminal: &TerminalRef) -> Option<&str> {
        self.terminals
            .iter()
            .find(|session| session.terminal().fences(terminal))
            .and_then(TerminalSession::error)
    }
}

/// welcome のメニュー操作を画面遷移へ写す。
fn welcome_action(action: MenuAction) -> WelcomeStep {
    match action {
        MenuAction::Quit => WelcomeStep::Quit,
        MenuAction::Open => WelcomeStep::OpenList,
        MenuAction::OpenRecent(index) => WelcomeStep::OpenRecent(index),
        MenuAction::New => WelcomeStep::NewForm,
        MenuAction::Config => WelcomeStep::ConfigScreen,
    }
}

/// Config 画面のキー処理。Save は dirty な Save 行でのみ有効で、Enter は save フローを
/// 開始（loading）する。保存中の再入力は `begin_save` が弾く。
#[allow(clippy::needless_pass_by_value)]
fn step_config(config: &mut Config, key: Key, settings: &mut dyn SettingsPort) -> ConfigStep {
    if config.is_selecting_team() {
        match key {
            Key::Left | Key::Char('h') => config.cycle_team_card(false),
            Key::Right | Key::Char('l') => config.cycle_team_card(true),
            Key::Up | Key::Char('k') => config.move_team_picker_vertical(false),
            Key::Down | Key::Char('j') => config.move_team_picker_vertical(true),
            Key::Enter => config.apply_team_picker(),
            Key::Escape => config.cancel_team_picker(),
            _ => {}
        }
        return ConfigStep::Stay;
    }
    if config.is_editing_environment() {
        match key {
            Key::Management {
                action: AppKey::SaveRoles,
                ..
            } if config.scope() == usagi_core::usecase::settings::SettingsScope::Global => {
                if settings.background_operations() {
                    return ConfigStep::SaveEnvironment;
                }
                config.save_environment(settings);
            }
            Key::Enter if config.is_environment_save_focused() => {
                if settings.background_operations() {
                    return ConfigStep::SaveEnvironment;
                }
                config.save_environment(settings);
            }
            Key::Enter => config.newline_environment(),
            Key::Tab => config.toggle_environment_focus(),
            Key::Backspace => config.backspace_environment(),
            Key::Delete => config.delete_environment(),
            Key::Left => config.move_environment(false),
            Key::Right => config.move_environment(true),
            Key::Up => config.move_environment_vertical(false),
            Key::Down => config.move_environment_vertical(true),
            Key::Home | Key::LineStart => config.move_environment_edge(false),
            Key::End | Key::LineEnd => config.move_environment_edge(true),
            Key::Char(character) if !character.is_control() => {
                config.type_environment(&character.to_string());
            }
            Key::Paste(text) => config.paste_environment(&text),
            Key::Escape => config.cancel_environment(),
            _ => {}
        }
        return ConfigStep::Stay;
    }
    match key {
        Key::Up | Key::Char('k') => {
            config.previous_field();
            ConfigStep::Stay
        }
        Key::Down | Key::Char('j') => {
            config.next_field();
            ConfigStep::Stay
        }
        Key::Left | Key::Char('h') => {
            config.cycle_selected(false);
            ConfigStep::Stay
        }
        Key::Right | Key::Char('l') => {
            config.cycle_selected(true);
            ConfigStep::Stay
        }
        // Enter begins the save flow (loading). `begin_save` is a no-op unless a
        // dirty Save row is focused with no save already in flight, so a rapid
        // second Enter cannot start a second save.
        Key::Enter if config.open_environment(settings) => ConfigStep::Stay,
        Key::Enter if config.open_team_picker() => ConfigStep::Stay,
        Key::Enter if config.begin_save() => ConfigStep::Save,
        Key::Escape => ConfigStep::Back,
        Key::Quit | Key::CtrlQ => ConfigStep::Quit,
        _ => ConfigStep::Stay,
    }
}

/// Workspace Config is an overlay owned by Home, so global quit chords must not
/// escape to the enclosing workspace loop while it has input focus. The full
/// screen Config keeps its existing quit contract through [`step_config`].
fn step_workspace_config(
    config: &mut Config,
    key: Key,
    settings: &mut dyn SettingsPort,
) -> WorkspaceConfigStep {
    match step_config(config, key, settings) {
        ConfigStep::Stay | ConfigStep::Quit => WorkspaceConfigStep::Stay,
        ConfigStep::Back => WorkspaceConfigStep::Back,
        ConfigStep::Save => WorkspaceConfigStep::Save,
        ConfigStep::SaveEnvironment => WorkspaceConfigStep::SaveEnvironment,
    }
}

/// Run Config from an opened workspace. The form contains only workspace-owned
/// settings and returns to the still-live Home runtime after Escape or save.
fn run_workspace_config(
    term: &mut dyn Terminal,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
    branches: &[BranchChoice],
    base: &[String],
) -> io::Result<()> {
    let mut form = Config::load_workspace_with_available_models_and_branches(
        settings,
        available_models,
        branches,
    );
    let mut help = None;
    loop {
        let (height, width) = term.size()?;
        let frame = config::render_over(height, width, base, &form);
        let frame = if let Some(help) = help {
            key_help::render_over(height, width, &frame, help)
        } else {
            frame
        };
        term.draw(&frame)?;
        let key = term.read_key()?;
        if let Some(state) = help.as_mut() {
            if matches!(key, Key::Help | Key::Escape) {
                help = None;
            } else {
                let _ = scroll_key_help(state, &key, height);
            }
            continue;
        }
        if key == Key::Help {
            help = Some(key_help::State::new(
                config_help_context(&form),
                WorkMode::Classic,
            ));
            continue;
        }
        match step_workspace_config(&mut form, key, settings) {
            WorkspaceConfigStep::Stay => {}
            WorkspaceConfigStep::Back => return Ok(()),
            WorkspaceConfigStep::Save => {
                if save_config_responsive(term, &mut form, settings, Some(base))? {
                    let (height, width) = term.size()?;
                    term.draw(&config::render_over(height, width, base, &form))?;
                    term.wait(config::DONE_DISPLAY)?;
                    form.reset_save();
                    return Ok(());
                }
            }
            WorkspaceConfigStep::SaveEnvironment => {
                let _ = save_environment_responsive(term, &mut form, settings);
            }
        }
    }
}

fn config_help_context(config: &Config) -> KeyHelpContext {
    if config.is_selecting_team() {
        KeyHelpContext::TeamPicker
    } else if config.is_editing_environment() {
        KeyHelpContext::EnvironmentEditor
    } else {
        KeyHelpContext::Config
    }
}

/// welcome 画面のキー処理。最上位画面なので Esc も終了として扱う。
#[allow(clippy::needless_pass_by_value)]
fn step_welcome(welcome: &mut Welcome, key: Key) -> WelcomeStep {
    match key {
        Key::Up | Key::Char('k') => {
            welcome.select_prev();
            WelcomeStep::Stay
        }
        Key::Down | Key::Char('j') => {
            welcome.select_next();
            WelcomeStep::Stay
        }
        Key::Escape | Key::Quit | Key::CtrlQ => WelcomeStep::Quit,
        Key::Enter => welcome_action(welcome.selected_action()),
        Key::Char(ch) => welcome
            .action_for(ch)
            .map_or(WelcomeStep::Stay, welcome_action),
        Key::Left
        | Key::Right
        | Key::PageUp
        | Key::PageDown
        | Key::Home
        | Key::End
        | Key::Delete
        | Key::LineStart
        | Key::LineEnd
        | Key::SelectLeft
        | Key::SelectRight
        | Key::SelectHome
        | Key::SelectEnd
        | Key::Backspace
        | Key::Tab
        | Key::CtrlD
        | Key::CtrlX
        | Key::Help
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Management { .. }
        | Key::Paste(_)
        | Key::TerminalCopy { .. }
        | Key::Resize
        | Key::Other => WelcomeStep::Stay,
    }
}

/// New 画面のキー処理（純粋）。矢印キーでフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
#[allow(clippy::needless_pass_by_value)]
fn step_new(form: &mut New, key: Key) -> NewStep {
    if form.is_creating() {
        return match key {
            Key::Escape => NewStep::Back,
            Key::Quit | Key::CtrlQ => NewStep::Quit,
            Key::Other | Key::Resize => {
                form.advance_create_animation();
                NewStep::Stay
            }
            _ => NewStep::Stay,
        };
    }
    match key {
        Key::Up => {
            form.focus_prev();
            NewStep::Stay
        }
        Key::Down => {
            form.focus_next();
            NewStep::Stay
        }
        Key::Left => {
            step_new_horizontal(form, false);
            NewStep::Stay
        }
        Key::Right => {
            step_new_horizontal(form, true);
            NewStep::Stay
        }
        // Home/End と emacs 行頭/行末（Ctrl-A/Ctrl-E）はフォーカス中フィールドの
        // キャレット移動。テキスト入力にフォーカスがあるので new-session ではなく caret。
        Key::Home | Key::LineStart => {
            form.cursor_home();
            NewStep::Stay
        }
        Key::End | Key::LineEnd => {
            form.cursor_end();
            NewStep::Stay
        }
        Key::SelectLeft => {
            form.select_left();
            NewStep::Stay
        }
        Key::SelectRight => {
            form.select_right();
            NewStep::Stay
        }
        Key::SelectHome => {
            form.select_home();
            NewStep::Stay
        }
        Key::SelectEnd => {
            form.select_end();
            NewStep::Stay
        }
        Key::Backspace => {
            form.backspace();
            NewStep::Stay
        }
        Key::Delete => {
            form.delete_forward();
            NewStep::Stay
        }
        Key::Char(ch) => {
            form.insert_char(ch);
            NewStep::Stay
        }
        // A bracketed paste inserts its text into the focused field verbatim, so
        // a repository URL or path pastes as one block.
        Key::Paste(text) => {
            for ch in text.chars() {
                form.insert_char(ch);
            }
            NewStep::Stay
        }
        Key::Escape => NewStep::Back,
        Key::Quit | Key::CtrlQ => NewStep::Quit,
        Key::Tab => form
            .begin_directory_completion()
            .map_or(NewStep::Stay, NewStep::CompleteDirectory),
        // Enter は入力を検証して作成へ進む。必須項目が欠けていれば安全なメッセージを
        // notice に出し、同画面に留まって draft を保つ。
        Key::Enter => match form.to_request() {
            Ok(request) => NewStep::Create(request),
            Err(error) => {
                form.set_notice(Some(error.message().to_owned()));
                NewStep::Stay
            }
        },
        Key::CtrlD
        | Key::CtrlX
        | Key::Help
        | Key::PageUp
        | Key::PageDown
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Management { .. }
        | Key::TerminalCopy { .. }
        | Key::Resize
        | Key::Other => NewStep::Stay,
    }
}

/// 作成失敗の io error を、New フォームの 1 行 notice slot に収まる安全なメッセージへ縮める。
/// git の stderr は複数行になりうるので先頭行だけを取り、長すぎる場合は切り詰める。
fn new_project_notice(error: &io::Error) -> String {
    const MAX: usize = 72;
    let message = error.to_string();
    let first = message.lines().next().unwrap_or("").trim();
    let detail = if first.is_empty() {
        "could not create the project"
    } else {
        first
    };
    if detail.chars().count() > MAX {
        let truncated: String = detail.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        detail.to_owned()
    }
}

/// New 画面の ←→ 操作。モード選択にフォーカスがあるときはモードを切り替え、テキスト欄では
/// キャレットを左右へ動かす（`right` が右方向）。
fn step_new_horizontal(form: &mut New, right: bool) {
    if form.focus() == Field::Mode {
        form.toggle_mode();
    } else if right {
        form.cursor_right();
    } else {
        form.cursor_left();
    }
}

/// Open 画面のキー処理。Enter で選択 path を確定し、Esc で welcome へ戻る。
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn step_open(open: &mut Open, key: Key) -> OpenStep {
    if open.unregistering_path().is_some() {
        return match key {
            Key::Left | Key::Right | Key::Tab => {
                open.toggle_unregister_choice();
                OpenStep::Stay
            }
            Key::Char('y' | 'Y') | Key::Enter => open
                .confirm_unregister()
                .map_or(OpenStep::Stay, OpenStep::ConfirmUnregister),
            Key::Char('n' | 'N') | Key::Escape => {
                open.cancel_unregister();
                OpenStep::Stay
            }
            Key::Quit | Key::CtrlQ => OpenStep::Quit,
            _ => OpenStep::Stay,
        };
    }
    if open.cleanup_confirming() {
        return match key {
            Key::Char('y') | Key::Enter => OpenStep::ConfirmCleanup,
            Key::Char('n') | Key::Escape => {
                open.cancel_cleanup();
                OpenStep::Stay
            }
            Key::Quit | Key::CtrlQ => OpenStep::Quit,
            _ => OpenStep::Stay,
        };
    }
    match key {
        Key::Up => {
            open.select_prev();
            OpenStep::Stay
        }
        Key::Down => {
            open.select_next();
            OpenStep::Stay
        }
        Key::Backspace => {
            open.pop_filter();
            OpenStep::Stay
        }
        Key::Left => {
            open.filter_left();
            OpenStep::Stay
        }
        Key::Right => {
            open.filter_right();
            OpenStep::Stay
        }
        Key::Home | Key::LineStart => {
            open.filter_home();
            OpenStep::Stay
        }
        Key::End | Key::LineEnd => {
            open.filter_end();
            OpenStep::Stay
        }
        Key::Delete => {
            open.filter_delete_forward();
            OpenStep::Stay
        }
        Key::SelectLeft => {
            open.filter_select_left();
            OpenStep::Stay
        }
        Key::SelectRight => {
            open.filter_select_right();
            OpenStep::Stay
        }
        Key::SelectHome => {
            open.filter_select_home();
            OpenStep::Stay
        }
        Key::SelectEnd => {
            open.filter_select_end();
            OpenStep::Stay
        }
        Key::Escape => OpenStep::Back,
        Key::Quit | Key::CtrlQ => OpenStep::Quit,
        Key::Enter => {
            let paths = if open.is_unite() {
                open.unite_paths()
            } else {
                open.selected()
                    .map(|workspace| vec![workspace.path.clone()])
                    .unwrap_or_default()
            };
            if paths.is_empty() {
                OpenStep::Stay
            } else {
                OpenStep::Choose(paths)
            }
        }
        Key::Tab => {
            open.toggle_unite();
            OpenStep::Stay
        }
        Key::Char(' ') if open.is_unite() => {
            open.toggle_unite_member();
            OpenStep::Stay
        }
        Key::Char('C') => {
            open.request_cleanup();
            OpenStep::Stay
        }
        Key::CtrlX => {
            open.request_unregister();
            OpenStep::Stay
        }
        Key::Char(ch) => {
            open.push_filter(ch);
            OpenStep::Stay
        }
        // A bracketed paste appends its text to the filter one character at a time.
        Key::Paste(text) => {
            for ch in text.chars() {
                open.push_filter(ch);
            }
            OpenStep::Stay
        }
        Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Management { .. }
        | Key::TerminalCopy { .. }
        | Key::CtrlD
        | Key::Help
        | Key::PageUp
        | Key::PageDown
        | Key::Resize
        | Key::Other => OpenStep::Stay,
    }
}

/// Run one daemon-owned session command without blocking the terminal event
/// loop. Admission is bounded to one worker; a concurrent request completes as
/// Busy without reaching the shared daemon port.
fn begin_session_command(
    ui: &mut WorkspaceIoRuntime,
    command: SessionCommand,
    completion: SessionBackendCompletion,
) -> bool {
    if ui.active_session_command.is_some() {
        emit_session_command_result(
            &Err("session command is already running".to_owned()),
            &completion,
        );
        return false;
    }
    let command_id = ui.next_session_command;
    ui.next_session_command = ui.next_session_command.wrapping_add(1);
    ui.active_session_command = Some(command_id);
    let port = std::sync::Arc::clone(&ui.session_commands);
    let workspace = ui.workspace.record().clone();
    let sender = ui.session_completion_sender.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            port.execute(&workspace, None, command)
        }))
        .unwrap_or_else(|_| Err("session command worker failed".to_owned()));
        // Complete the reducer request before returning the projection/port to
        // the UI. If the workspace exited, the sink is closed harmlessly but
        // the accepted Effect still took exactly one completion path.
        emit_session_command_result(&result, &completion);
        let _ = sender.send(SessionCommandCompletion {
            command_id,
            result,
            completion,
        });
    });
    true
}

/// The daemon-owned name for the session identified by `session`, if the current
/// sidebar projection still holds it. A `RemoveSession` effect carries the stable
/// identity, while the session command port speaks the daemon-facing name.
fn session_name_for(ui: &WorkspaceIoRuntime, session: SessionId) -> Option<String> {
    ui.workspace
        .session_ids()
        .iter()
        .zip(ui.workspace.sessions())
        .find_map(|(id, record)| (*id == session).then(|| record.name.clone()))
}

/// Reconcile sidebar rows and the IDs used by Agent/terminal requests as one
/// daemon-authoritative observation. Rows without a complete, unique identity
/// set are dropped so no display name or stale ID can become an action target.
fn apply_session_projection(
    ui: &mut WorkspaceIoRuntime,
    sessions: Option<Vec<usagi_core::domain::session::SessionRecord>>,
    session_ids: Option<Vec<SessionId>>,
    agent_resumes: Option<BTreeMap<SessionId, ProviderResumeProjection>>,
    session_lifecycles: Option<BTreeMap<SessionId, SessionLifecycleProjection>>,
    session_roles: Option<BTreeMap<SessionId, SessionRoleProjection>>,
) {
    let Some(sessions) = sessions else {
        return;
    };
    let lifecycles = session_lifecycles.unwrap_or_default();
    if let Some(session_ids) =
        session_ids.filter(|ids| runtime_identities_are_valid(sessions.len(), ids))
    {
        ui.workspace
            .replace_sessions_with_runtime_ids(sessions, session_ids.clone());
        if let Some(agent) = ui.agent.as_mut() {
            // Only usable (attachable) sessions can host an Agent. A Failed row
            // owns its name and is now listed, but must never become an Agent
            // launch target, so gate the allowed set by `can_use`. A session with
            // no lifecycle entry stays allowed as before.
            agent.sessions = session_ids
                .iter()
                .copied()
                .filter(|id| {
                    lifecycles
                        .get(id)
                        .is_none_or(|projection| projection.capabilities().can_use)
                })
                .collect();
        }
    } else {
        // Rows without daemon-issued identities are not actionable. Drop the
        // whole observation instead of retaining or fabricating stale targets.
        ui.workspace
            .replace_sessions_with_runtime_ids(Vec::new(), Vec::new());
        if let Some(agent) = ui.agent.as_mut() {
            agent.sessions.clear();
        }
    }
    ui.workspace.set_session_lifecycles(lifecycles);
    ui.workspace
        .set_session_roles(session_roles.unwrap_or_default());
    if let Some(agent_resumes) = agent_resumes {
        ui.agent_resumes = agent_resumes;
    }
}

/// Receive completed create/remove workers before drawing the next frame. The
/// returned port is reclaimed for the next command and a successful daemon
/// snapshot is reconciled into the session cache, which [`sync_runtime_sessions`]
/// then promotes into the controller's Home rows. A failure is no longer dropped
/// silently: the port's message is display-safe by contract and is collapsed to a
/// safe single line before it reaches the screen. A create failure refluxes as a
/// failed [`OperationResult`] so its pending row clears and the safe message opens
/// the create-failure dialog; any other failure (e.g. remove) refluxes as a
/// controller [`BackendEvent::Notice`]. Both are distinct from an in-form local
/// validation error.
fn drain_session_completions(ui: &mut WorkspaceIoRuntime) {
    let completions = ui
        .session_completions
        .try_iter()
        .take(FRAME_EVENT_BUDGET)
        .collect::<Vec<_>>();
    for completion in completions {
        if ui.active_session_command != Some(completion.command_id) {
            continue;
        }
        ui.active_session_command = None;
        match &completion.completion {
            SessionBackendCompletion::Create { .. } => ui.creating_session = None,
            SessionBackendCompletion::Remove { session, .. }
                if ui.removing_session == Some(*session) =>
            {
                ui.removing_session = None;
            }
            SessionBackendCompletion::Remove { .. } | SessionBackendCompletion::Sleep { .. } => {}
        }
        if let Ok(result) = completion.result {
            adopt_session_snapshot(ui, result);
        }
    }
}

/// Reconcile one daemon lifecycle snapshot into the session cache, ignoring a
/// snapshot older than one already adopted.
///
/// The revision gate is what makes the resident lane's coalescing safe: an
/// observation that started before a user's create/remove but landed after it
/// carries the older revision and is discarded, so the newest daemon state wins
/// regardless of which lane observed it (#551).
fn adopt_session_snapshot(ui: &mut WorkspaceIoRuntime, result: SessionCommandResult) {
    let is_current = result
        .revision
        .is_none_or(|revision| revision >= ui.last_session_revision);
    if let Some(revision) = result.revision.filter(|_| is_current) {
        ui.last_session_revision = revision;
    }
    if is_current {
        apply_session_projection(
            ui,
            result.sessions,
            result.session_ids,
            result.agent_resumes,
            result.session_lifecycles,
            result.session_roles,
        );
    }
}

/// Drain the resident session-inventory lane and complete the refresh requests
/// parked on it.
///
/// This is the whole of what the frame loop does for the session lane: no
/// connection, no request, no worker spawn. Everything parked in
/// `pending_session_refresh` completes against the one snapshot the lane
/// published, which is how several `RefreshSessions` effects inside one cadence
/// period cost exactly one daemon request (#551).
fn drain_session_refresh(
    ui: &mut WorkspaceIoRuntime,
    session_refresh: &mut dyn SessionRefreshPort,
    pending_session_refresh: &mut Option<Completions>,
) {
    let Some(result) = session_refresh.take() else {
        return;
    };
    match result {
        Ok(result) => {
            adopt_session_snapshot(ui, result);
            let ids = ui.workspace.session_ids().to_vec();
            if let Some(completions) = pending_session_refresh.take() {
                completions.emit(AppEvent::Backend(BackendEvent::Sessions(ids)));
            }
        }
        Err(message) => {
            if let Some(completions) = pending_session_refresh.take() {
                completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                    safe_session_error(&message),
                ))));
            }
        }
    }
}

/// Emit the exactly-one reducer completion owned by one admitted command.
/// Projection and port recovery are deliberately separate so workspace exit or
/// a closed host channel cannot strand controller pending state.
fn emit_session_command_result(
    result: &Result<SessionCommandResult, String>,
    completion: &SessionBackendCompletion,
) {
    match (result, completion) {
        (
            Ok(result),
            SessionBackendCompletion::Create {
                token,
                before,
                completions,
            },
        ) => {
            let created = result
                .session_ids
                .as_ref()
                .and_then(|ids| ids.iter().copied().find(|id| !before.contains(id)));
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: *token,
                succeeded: created.is_some(),
                created,
                notice: Some(Notice::new(if created.is_some() {
                    "session created"
                } else {
                    "daemon did not return the created session"
                })),
            }));
        }
        (
            Ok(result),
            SessionBackendCompletion::Remove {
                before,
                completions,
                ..
            }
            | SessionBackendCompletion::Sleep {
                before,
                completions,
            },
        ) => {
            completions.emit(AppEvent::Backend(BackendEvent::Sessions(
                result.session_ids.clone().unwrap_or_else(|| before.clone()),
            )));
        }
        (
            Err(message),
            SessionBackendCompletion::Create {
                token, completions, ..
            },
        ) => {
            completions.emit(AppEvent::OperationResult(OperationResult {
                token: *token,
                succeeded: false,
                created: None,
                notice: Some(Notice::new(safe_session_error(message))),
            }));
        }
        (
            Err(message),
            SessionBackendCompletion::Remove { completions, .. }
            | SessionBackendCompletion::Sleep { completions, .. },
        ) => {
            completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                safe_session_error(message),
            ))));
        }
    }
}

/// Collapse a daemon session-command error into a safe single line for the
/// create-failure dialog: take the first line only, so multi-line stderr or
/// internal detail on later lines never leaks onto the screen. The line is kept
/// in full — the dialog wraps it to the box width and shows all of it, so no
/// length cap truncates a legitimate error into an ellipsis.
fn safe_session_error(message: &str) -> String {
    let first = message.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        "could not create the session".to_owned()
    } else {
        first.to_owned()
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
fn enqueue_pane_launch(ui: &mut WorkspaceIoRuntime, launch: PaneLaunch) {
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
fn drain_pane_launches(ui: &mut WorkspaceIoRuntime, geometry: Geometry) {
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
fn run_pane_launch(
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

/// Translates a presentation [`Key`] into the controller's [`AppEvent`] vocabulary
/// for the real-terminal runtime that routes Home input through `update()`.
///
/// The composition-root adapter has already resolved the `Ctrl-O` live prefix, so
/// [`Key::Live`] arrives as a settled [`LiveTerminalAction`] that this function
/// maps to the equivalent [`AppKey`]. Ordinary keys map one-to-one; the reducer,
/// which owns overlay context, decides what each means. `Key::Other` and
/// `Key::Resize` (backend wakeups and terminal resizes the composition root
/// cannot express as input) advance the
/// mascot via [`AppEvent::Tick`] — real resize dimensions come from `term.size()`
/// and backend results from `DaemonBackend::drain_events()`, not from a `Key`.
///
/// Sidebar clicks need a monotonic timestamp and are adapted separately by
/// [`sidebar_pointer_event`]. Returns `None` for input the Home reducer never
/// consumes: raw PTY passthrough, pointer input, and keys with no Home management
/// meaning.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn app_event_from_key(key: Key) -> Option<AppEvent> {
    let app_key = match key {
        Key::Management { action, .. } => return Some(AppEvent::Key(action)),
        Key::Live(action) => return live_action_to_app_key(action).map(AppEvent::Key),
        Key::Resize | Key::Other => return Some(AppEvent::Tick),
        Key::Up => AppKey::Up,
        Key::Down => AppKey::Down,
        Key::PageUp => AppKey::PageUp,
        Key::PageDown => AppKey::PageDown,
        // Switch-mode Left/Right is consumed by the process-level project deck
        // before this mapping. When the keys reach the reducer they move a
        // horizontal choice such as the quit confirmation. Tab motion between
        // live tabs stays Ctrl-N/P.
        Key::Left => AppKey::Left,
        Key::Right => AppKey::Right,
        Key::Enter => AppKey::Enter,
        Key::Backspace => AppKey::Backspace,
        Key::Paste(text) => AppKey::Paste(text),
        Key::Tab => AppKey::Tab,
        Key::Escape => AppKey::Escape,
        // Runtime adapters preserve Ctrl-A as U+0001. `Ctrl-A` (LineStart) and
        // `Home` both mean `+ new session` here, where no text field owns focus:
        // the established sidebar-navigation contract that the reducer keeps intact.
        // A focused palette / create form intercepts these before the
        // reducer, so caret motion never reaches this navigation branch.
        Key::LineStart | Key::Home | Key::Char('\u{1}') => AppKey::CtrlA,
        Key::Char(character) => AppKey::Char(character),
        Key::Quit => AppKey::CtrlC,
        Key::CtrlQ => AppKey::CtrlQ,
        Key::CtrlX => AppKey::CtrlX,
        Key::Help => return None,
        Key::TerminalCopy { fallback } => {
            return {
                #[cfg(target_os = "windows")]
                {
                    let _ = fallback;
                    Some(AppEvent::Key(AppKey::CtrlC))
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = fallback;
                    None
                }
            };
        }
        // Input the Home reducer never consumes: raw PTY passthrough, terminal
        // pointer drags and clicks (a shell + `TerminalSession` concern), and the
        // caret/selection keys that have meaning only inside a focused text field
        // (End/Ctrl-E, Delete, Shift+arrows). Ctrl-D is terminal EOT and remains
        // inert on management surfaces.
        Key::Passthrough(_)
        | Key::Pointer(_)
        | Key::Click { .. }
        | Key::CtrlD
        | Key::End
        | Key::LineEnd
        | Key::Delete
        | Key::SelectLeft
        | Key::SelectRight
        | Key::SelectHome
        | Key::SelectEnd => {
            return None;
        }
    };
    Some(AppEvent::Key(app_key))
}

/// Maps a resolved live-terminal action to its Home reducer key. Tab close and
/// terminal scroll/copy stay pane- and shell-level concerns the Home reducer has
/// no vocabulary for, so they return `None`.
fn live_action_to_app_key(action: LiveTerminalAction) -> Option<AppKey> {
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

fn terminal_geometry(height: usize, width: usize) -> Geometry {
    let (rows, cols) = workspace::terminal_viewport(height, width);
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
fn managed_background_terminal_geometry(height: usize, width: usize) -> Geometry {
    terminal_geometry(height, width)
}

fn foreground_terminal_geometry(
    height: usize,
    width: usize,
    director_open: bool,
    root_terminal_open: bool,
    root_terminal_full_height: bool,
    focus: Option<WorkspaceDrawerFocus>,
) -> Geometry {
    if director_open && focus == Some(WorkspaceDrawerFocus::Director) {
        let viewport = director_drawer::terminal_viewport(height, width);
        Geometry {
            cols: u16::try_from(viewport.cols.min(usize::from(u16::MAX)))
                .expect("clamped drawer terminal width fits u16"),
            rows: u16::try_from(viewport.rows.min(usize::from(u16::MAX)))
                .expect("clamped drawer terminal height fits u16"),
        }
    } else if root_terminal_open {
        let available_width =
            workspace::root_terminal_available_width(height, width, director_open);
        let viewport = root_terminal_drawer::terminal_viewport_for_mode(
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
fn managed_background_terminal(runtime: &WorkspaceRuntime) -> Option<TerminalRef> {
    runtime.workspace_drawer_background_terminal()
}

/// Stable attachment set for Home and both workspace drawers.
/// The focused root surface comes first; retained background surfaces are
/// read-only and keep their non-overlay geometry.
fn workspace_terminal_attachments(
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

fn render_open(height: usize, width: usize, open: &Open, now: DateTime<Utc>) -> Vec<String> {
    let base = open::render(height, width, open, now);
    if let Some(path) = open.unregistering_path() {
        let title = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Unregister workspace");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint(&format!("Unregister {}?", path.display()));
        return modal::render_confirmation_over(
            height,
            width,
            &base,
            open.unregister_confirmation(),
            ConfirmationView::confirmation(
                &title,
                52,
                heading,
                "Only the registry entry is removed. Files stay.",
            ),
        );
    }
    // The cleanup prompt has no Yes/No focus toggle (y/Enter removes, n/Esc
    // cancels), so it flows through the shared confirmation renderer as a
    // compact, button-less variant. The state argument is unused when compact.
    if open.cleanup_confirming() {
        let title = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Clean up registry");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Remove missing registry entries?");
        return modal::render_confirmation_over(
            height,
            width,
            &base,
            modal::ConfirmationModal::new(),
            ConfirmationView::confirmation(
                &title,
                52,
                heading,
                "Registry entries whose folder is gone are removed.",
            )
            .compact("y: remove   n/Esc: cancel"),
        );
    }
    base
}

/// Ordered workspace paths represented by one Recent card.
fn recent_paths(recent: &Recent) -> Vec<PathBuf> {
    match recent {
        Recent::Workspace(overview) => vec![overview.workspace.path.clone()],
        Recent::Unite(unite) => unite
            .members()
            .iter()
            .map(|overview| overview.workspace.path.clone())
            .collect(),
    }
}

fn registry_contains_path(registry: &[Workspace], path: &Path) -> bool {
    for workspace in registry {
        if workspace.path == path {
            return true;
        }
    }
    false
}

fn remove_registry_paths(registry: &mut Vec<Workspace>, removed: &[PathBuf]) {
    let mut index = 0;
    while index < registry.len() {
        let mut matched = false;
        for path in removed {
            if registry[index].path == *path {
                matched = true;
                break;
            }
        }
        if matched {
            registry.remove(index);
        } else {
            index += 1;
        }
    }
}

#[cfg(test)]
thread_local! {
    static SESSION_PROJECTION_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TERMINAL_PROJECTION_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_projection_build_counts() {
    SESSION_PROJECTION_BUILDS.set(0);
    TERMINAL_PROJECTION_BUILDS.set(0);
}

#[cfg(test)]
fn projection_build_counts() -> (usize, usize) {
    (
        SESSION_PROJECTION_BUILDS.get(),
        TERMINAL_PROJECTION_BUILDS.get(),
    )
}

/// Project the daemon-authoritative session records into the controller's Home
/// row material, in the same order the runtime holds their IDs.
fn project_controller_sessions(ui: &WorkspaceIoRuntime, state: &AppState) -> Vec<ProjectedSession> {
    #[cfg(test)]
    SESSION_PROJECTION_BUILDS.set(SESSION_PROJECTION_BUILDS.get() + 1);
    let observed = ui
        .workspace
        .sessions()
        .iter()
        .zip(ui.workspace.session_ids())
        .map(|(record, id)| {
            let mut projected = ProjectedSession::from_record(*id, record);
            projected.removing = ui.removing_session == Some(*id);
            projected.agent_resume = ui.agent_resumes.get(id).copied();
            if let Some(projection) = ui.workspace.session_lifecycles().get(id) {
                projected.lifecycle = projection.lifecycle;
                projected
                    .failure_stage
                    .clone_from(&projection.failure_stage);
                projected
                    .failure_summary
                    .clone_from(&projection.failure_summary);
                // The daemon accepts a removal before its worktree teardown
                // runs, so a `Deleting` row is authoritatively still being
                // removed — by a worker that outlives this request and even this
                // process. Keep showing the removal affordance for as long as
                // the daemon says so, not only until the local command returns.
                projected.removing |= projection.lifecycle == SessionLifecycle::Deleting;
            }
            projected
        })
        .collect::<Vec<_>>();
    crate::presentation::views::workspace::project_sessions(state, &observed)
}

/// Render a single static Home frame from a workspace snapshot, using the same
/// controller projection as the interactive loop.
///
/// This is the non-interactive `usagi open <path>` fallback (no terminal), so
/// it shows the initial project bar and Home surface: root selected/active, the
/// snapshot's sessions, and the `+ new session` row. The composition root passes
/// the resolved global icon preference just as it does for the interactive loop.
#[must_use]
pub fn render_home_snapshot(
    height: usize,
    width: usize,
    snapshot: &WorkspaceSnapshot,
    icon_mode: IconMode,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(height, width);
    let workspace = WorkspaceView::with_runtime_ids(
        snapshot.workspace.clone(),
        snapshot.state.clone(),
        snapshot.session_ids.clone(),
    );
    let sessions: Vec<ProjectedSession> = workspace
        .sessions()
        .iter()
        .zip(workspace.session_ids())
        .map(|(record, id)| {
            let mut projected = ProjectedSession::from_record(*id, record);
            if let Some(projection) = snapshot.session_lifecycles.get(id) {
                projected.lifecycle = projection.lifecycle;
                projected
                    .failure_stage
                    .clone_from(&projection.failure_stage);
                projected
                    .failure_summary
                    .clone_from(&projection.failure_summary);
            }
            projected
        })
        .collect();
    let state = AppState::home(snapshot.workspace_id, snapshot.session_ids.clone());
    let projection = HomeProjection::from_state(&state, &snapshot.workspace.name, &sessions)
        .with_icon_mode(icon_mode);
    let mut frame = Vec::with_capacity(height);
    frame.push(project_bar(&WorkspaceDeck::new(snapshot), width).line);
    frame.extend(render_home(
        height.saturating_sub(PROJECT_BAR_ROWS),
        width,
        &projection,
    ));
    frame
}

/// Keep the controller's Home rows in step with the daemon session projection
/// the IO runtime reconciled this frame.
///
/// `worktree_names` is the inline create form's collision hint, supplied by
/// [`SessionWorktreeHint`]. It is empty while the form is closed, because the
/// scan that produces it is filesystem IO which must not ride the frame budget
/// (#554).
fn sync_runtime_sessions(
    runtime: &mut WorkspaceRuntime,
    ui: &WorkspaceIoRuntime,
    worktree_names: &[String],
) {
    let ids = ui.workspace.session_ids().to_vec();
    if runtime.state().sessions() != ids.as_slice() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Sessions(ids)));
    }
    // Keep the reducer's advisory name copy in step so the create form can reject
    // a known worktree collision locally before it ever reaches the daemon. The
    // lifecycle snapshot supplies managed sessions; the directory scan also
    // catches a stale `.usagi/sessions/<name>` that has no lifecycle record.
    let mut names: std::collections::BTreeSet<String> = ui
        .workspace
        .sessions()
        .iter()
        .map(|record| record.name.clone())
        .collect();
    names.extend(worktree_names.iter().cloned());
    let names: Vec<String> = names.into_iter().collect();
    if runtime.state().session_names() != names.as_slice() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionNames(names)));
    }
    // Keep the reducer's per-session lifecycle in step so it can gate attach
    // and recognize a typed delete failure without parsing display text.
    let lifecycles = ui.workspace.session_lifecycles().clone();
    if runtime.state().session_lifecycles() != &lifecycles {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionLifecycles(
            lifecycles,
        )));
    }
    if runtime.state().session_roles() != ui.workspace.session_roles() {
        let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionRoles(
            ui.workspace.session_roles().clone(),
        )));
    }
}

/// Test-only filesystem adapter. Production owns the equivalent adapter in the
/// binary composition root, outside this IO-free crate.
#[cfg(test)]
pub struct FsSessionWorktreeScanPort;

#[cfg(test)]
impl SessionWorktreeScanPort for FsSessionWorktreeScanPort {
    fn scan(&mut self, workspace: &Path) -> Vec<String> {
        let sessions = workspace.join(".usagi").join("sessions");
        std::fs::read_dir(sessions)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(std::fs::FileType::is_dir)
                    .map(|_| entry)
            })
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect()
    }
}

struct UnavailableSessionWorktreeScanPort;

impl SessionWorktreeScanPort for UnavailableSessionWorktreeScanPort {
    fn scan(&mut self, _: &Path) -> Vec<String> {
        Vec::new()
    }
}

/// Cadence gate that keeps the create form's collision hint off the frame
/// budget.
///
/// Before #554 the frame loop scanned `<workspace>/.usagi/sessions` on every
/// tick — about 62 `read_dir` calls plus one `stat` per entry every second,
/// growing with the session count, for a hint only the inline create form ever
/// reads. The scan now runs on the frame that opens the form and then at most
/// once per [`Self::CADENCE`] while it stays open; closing the form drops the
/// hint and stops the IO entirely.
///
/// Staleness is safe: the daemon re-checks the name when the request is
/// submitted and rejects a collision this hint missed, so the hint only has to
/// be good enough to catch the common case before the round trip.
struct SessionWorktreeHint {
    scan: Box<dyn SessionWorktreeScanPort>,
    names: Vec<String>,
    /// Elapsed time of the last scan, cleared whenever the form closes so the
    /// next opening always sees a freshly scanned hint.
    scanned_at: Option<std::time::Duration>,
}

impl SessionWorktreeHint {
    /// Ceiling on how often a form left open re-reads the directory.
    const CADENCE: std::time::Duration = std::time::Duration::from_millis(500);

    fn new(scan: Box<dyn SessionWorktreeScanPort>) -> Self {
        Self {
            scan,
            names: Vec::new(),
            scanned_at: None,
        }
    }

    /// The hint to fold into the reducer's advisory name copy this frame.
    ///
    /// Returns an empty slice while `form_open` is false, and never scans then.
    fn names(&mut self, form_open: bool, workspace: &Path, now: std::time::Duration) -> &[String] {
        if !form_open {
            self.names.clear();
            self.scanned_at = None;
            return &self.names;
        }
        let due = self
            .scanned_at
            .is_none_or(|last| now.saturating_sub(last) >= Self::CADENCE);
        if due {
            self.names = self.scan.scan(workspace);
            self.scanned_at = Some(now);
        }
        &self.names
    }
}

/// Project the focused live terminal's already-polled rows for
/// `with_terminal_view`, folding in the shell-owned scroll offset, selection
/// highlight, and copy feedback tracked by `controls`. Focus changes select the
/// matching terminal-local state; tabs no longer present in the registry are
/// pruned from the bounded cache.
fn controller_terminal_view(
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

/// Project the root pane entry into the frontmost Agent-only drawer.
///
/// Stable identity and selection remain in the pane/intent reducers. This
/// adapter exposes only safe labels and the already-rendered VT rows.
fn director_drawer_projection(
    ui: &WorkspaceIoRuntime,
    runtime: &WorkspaceRuntime,
    terminal_view: Option<&TerminalViewProjection>,
) -> DirectorDrawerProjection {
    if !runtime.state().director_drawer_open() {
        return DirectorDrawerProjection::default();
    }
    let pane = runtime.active_pane();
    let selected = runtime.director_selection();
    let mut conversations = Vec::new();
    for tab in pane.tabs() {
        let conversation = match tab {
            PaneTab::Live(live) if live.kind == PaneKind::Agent => Some(DirectorConversation {
                label: AgentTabIntent::safe_label_or_fallback(
                    ui.agent_continuation_for(&live.terminal),
                ),
                selected: matches!(
                    selected.as_ref(),
                    Some(TabSelection::Live(terminal)) if terminal.fences(&live.terminal)
                ),
            }),
            PaneTab::Interrupted(interrupted) => Some(DirectorConversation {
                label: interrupted.tab.safe_label(),
                selected: matches!(
                    selected.as_ref(),
                    Some(TabSelection::Interrupted(continuation))
                        if *continuation == interrupted.tab.continuation
                ),
            }),
            PaneTab::Pending(pending) if pending.kind == PaneKind::Agent => {
                Some(DirectorConversation {
                    label: "Agent (starting)".to_owned(),
                    selected: matches!(
                        selected.as_ref(),
                        Some(TabSelection::Pending(operation)) if *operation == pending.operation
                    ),
                })
            }
            PaneTab::Live(_) | PaneTab::Pending(_) | PaneTab::Ready(_) => None,
        };
        if let Some(conversation) = conversation {
            conversations.push(conversation);
        }
    }
    let mut terminal_view = terminal_view.cloned();
    if let Some(view) = &mut terminal_view
        && view.feedback.is_none()
    {
        view.feedback = pane.error().map(str::to_owned);
    }
    let interrupted_detail = if terminal_view.is_none() {
        runtime
            .focused_interrupted()
            .map(|interrupted| interrupted.safe_detail().to_owned())
    } else {
        None
    };
    // Management routes do not draw `terminal_view`, but a root launch can
    // fail while an older live Director remains selected. Keep the pane-safe
    // reason at the route level as well as in the Console terminal projection.
    let feedback = pane.error().map(str::to_owned);
    let goal_driven =
        runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven;
    let organization = if !goal_driven
        && conversations
            .iter()
            .any(|conversation| conversation.selected)
    {
        director_organization(ui)
    } else {
        Vec::new()
    };
    if goal_driven {
        conversations.clear();
    }
    DirectorDrawerProjection {
        focused: runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director),
        route: runtime.state().director_route(),
        goal_driven,
        conversations,
        organization,
        terminal_view,
        interrupted_detail,
        feedback,
        new: director_new_projection(runtime),
        work_runs: WorkRunProjection::default(),
        work_run_control: WorkRunControlProjection::default(),
    }
}

fn director_new_projection(runtime: &WorkspaceRuntime) -> DirectorNewProjection {
    if runtime.state().director_launching().is_some() {
        return DirectorNewProjection::Launching;
    }
    match runtime.state().director_new() {
        DirectorNew::Idle => DirectorNewProjection::Ready,
        DirectorNew::Empty => DirectorNewProjection::Empty,
        DirectorNew::Choosing(selected) => {
            let candidates = runtime
                .state()
                .available_models()
                .iter()
                .map(|model| model.selector().to_owned())
                .collect::<Vec<_>>();
            let selected = runtime
                .state()
                .available_models()
                .iter()
                .position(|model| model == selected)
                .unwrap_or(0);
            if runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven {
                DirectorNewProjection::GoalComposer {
                    candidates,
                    selected,
                    goal: runtime.state().director_goal().to_owned(),
                }
            } else {
                DirectorNewProjection::Choosing {
                    candidates,
                    selected,
                }
            }
        }
    }
}

fn director_organization(ui: &WorkspaceIoRuntime) -> Vec<DirectorOrganizationRow> {
    fn append_children(
        parent: Option<SessionId>,
        depth: usize,
        members: &[(SessionId, Option<SessionId>, DirectorOrganizationRow)],
        emitted: &mut std::collections::BTreeSet<SessionId>,
        rows: &mut Vec<DirectorOrganizationRow>,
    ) {
        for (id, member_parent, row) in members {
            if *member_parent == parent && emitted.insert(*id) {
                let mut row = row.clone();
                row.depth = depth;
                rows.push(row);
                append_children(Some(*id), depth.saturating_add(1), members, emitted, rows);
            }
        }
    }

    let roles = ui.workspace.session_roles();
    let mut members = Vec::new();
    for (session_id, session) in ui
        .workspace
        .session_ids()
        .iter()
        .zip(ui.workspace.sessions())
    {
        let role_identity = roles
            .get(session_id)
            .and_then(|role| role.role_id.as_ref())
            .map_or_else(
                || "• Executor".to_owned(),
                |role| views::workspace::role_identity(role.as_str()),
            );
        let status = match roles.get(session_id).and_then(|role| role.agent_status) {
            Some(usagi_core::domain::agent::AgentStatus::Starting) => "starting",
            Some(usagi_core::domain::agent::AgentStatus::Running) => "running",
            Some(usagi_core::domain::agent::AgentStatus::Idle) => "waiting",
            Some(usagi_core::domain::agent::AgentStatus::Exited) => "stopped",
            Some(usagi_core::domain::agent::AgentStatus::Failed) => "failed",
            None => "ready",
        };
        let row = DirectorOrganizationRow {
            depth: 0,
            label: format!("{role_identity} · {}", session.name),
            status: status.to_owned(),
        };
        members.push((
            *session_id,
            roles
                .get(session_id)
                .and_then(|role| role.parent_session_id),
            row,
        ));
    }
    let mut rows = vec![DirectorOrganizationRow {
        depth: 0,
        label: format!("{} Director", director_drawer::DIRECTOR_ICON),
        status: "active".into(),
    }];
    let mut emitted = std::collections::BTreeSet::new();
    append_children(None, 1, &members, &mut emitted, &mut rows);
    // Corrupt or retention-truncated parentage is still visible, but never
    // allowed to form an unbounded/cyclic presentation walk.
    for (id, _, row) in members {
        if emitted.insert(id) {
            let mut row = row;
            row.depth = 1;
            rows.push(row);
        }
    }
    rows
}

/// Run the per-frame visible-terminal sweep: poll the attached selection(s),
/// auto-close them if exited, then project the focused viewport. Returns
/// the projection plus its `(rows_len, scroll)` so a later pointer drag maps back
/// to the exact retained cell.
#[cfg(test)]
fn poll_and_project_terminals(
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

fn sync_terminal_selection_motions(
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
fn close_exited_panes(ui: &mut WorkspaceIoRuntime, runtime: &mut WorkspaceRuntime) {
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
fn shell_target_for_terminal(terminal: &TerminalRef) -> Target {
    terminal
        .session_id
        .map_or(Target::Root(terminal.workspace_id), Target::Session)
}

/// Run restore over a dedicated daemon port. Inventory is retried with bounded
/// backoff on this worker, so the first frame and terminal input loop never wait
/// for a handshake or a slow daemon response.
fn spawn_restore_job(
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

fn normalize_terminal_inventory(entries: &mut Vec<TerminalInventoryEntry>) {
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

fn restore_inventory_is_coherent(
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

fn pane_restore_targets(
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

fn terminal_restore_sort_key(terminal: &TerminalRef) -> (String, String, String, String, String) {
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
fn generic_restore_targets(
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

fn apply_restore_completion(
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
    let targets = pane_restore_targets(
        workspace,
        allowed_sessions,
        observation.projection,
        &restorable,
        selected.as_ref(),
        interrupted,
        &saved_selections,
    );
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
fn restore_open_panes(
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

/// Close the focused pane tab (Ctrl-O x / Ctrl-O Ctrl-X) and perform the daemon transport work:
/// request a live process exit, or drop a still-pending launch (both its queued
/// work and its completion routing) so it cannot spawn a detached daemon
/// terminal behind the vanished placeholder.
fn close_focused_terminal_pane(
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

/// Commit one exact interrupted-lineage dismissal before removing its local
/// tab. The stable target protects modal confirmation from closing a different
/// tab if a background projection changed focus meanwhile.
fn dismiss_interrupted_history(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    target: Target,
    interrupted: InterruptedTab,
) {
    if let Err(error) = ui.mutate_agent_intent(AgentTabIntentMutation::DismissInterrupted {
        session_id: interrupted.session_id,
        continuation: interrupted.continuation,
        terminal: interrupted.last_terminal,
    }) {
        surface_agent_tab_intent_error(runtime, error);
        return;
    }
    runtime.dismiss_interrupted_tab(target, interrupted.continuation);
}

fn surface_agent_tab_intent_error(runtime: &mut WorkspaceRuntime, error: AgentTabIntentError) {
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(Notice::new(
        error.safe_message(),
    ))));
}

/// Drive the complete terminal-output pointer gesture in one place. Down records
/// a snapshot and anchor without selecting, the first Drag promotes it to a text
/// selection, and Up resolves to exactly one of copy or link-open. `rows_len` /
/// `scroll` describe the frame's projected viewport so every phase maps back to
/// the exact retained cell.
#[allow(clippy::too_many_arguments)]
fn handle_terminal_pointer(
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
            director_drawer::terminal_point_at(height, width, rows_len, scroll, column, row)
        } else if runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Terminal) {
            root_terminal_drawer::terminal_point_at_for_mode(
                height,
                width,
                workspace::root_terminal_available_width(
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
        PointerKind::Down => {
            if !runtime.wants_live_input() {
                return false;
            }
            let terminal = runtime
                .focused_terminal()
                .expect("live input ownership requires a selected live terminal");
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
fn copy_terminal_selection(controls: &mut LiveTerminalControls, term: &mut dyn Terminal) {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectorTabSelection {
    Unhandled,
    Handled,
    Selected,
}

fn select_director_tab_outcome(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> DirectorTabSelection {
    if !runtime.state().director_drawer_open() {
        return DirectorTabSelection::Unhandled;
    }
    let direction = match key {
        Key::Live(LiveTerminalAction::NextTab) => {
            crate::usecase::application::controller::TabDirection::Next
        }
        Key::Live(LiveTerminalAction::PreviousTab) => {
            crate::usecase::application::controller::TabDirection::Previous
        }
        Key::Down if runtime.state().director_route() == DirectorRoute::Organization => {
            crate::usecase::application::controller::TabDirection::Next
        }
        Key::Up if runtime.state().director_route() == DirectorRoute::Organization => {
            crate::usecase::application::controller::TabDirection::Previous
        }
        _ => return DirectorTabSelection::Unhandled,
    };
    let Some(selection) = runtime.agent_selection_after_select(direction) else {
        return DirectorTabSelection::Handled;
    };
    let continuation = match &selection {
        TabSelection::Live(terminal) => ui.agent_continuation_for(terminal),
        TabSelection::Interrupted(continuation) => Some(*continuation),
        TabSelection::Pending(_) | TabSelection::Ready(_) => None,
    };
    match ui.mutate_agent_intent(AgentTabIntentMutation::Select {
        session_id: None,
        continuation,
    }) {
        Ok(()) => {
            let _ = runtime.select_tab_selection(selection);
            DirectorTabSelection::Selected
        }
        Err(error) => {
            surface_agent_tab_intent_error(runtime, error);
            DirectorTabSelection::Handled
        }
    }
}

#[cfg(test)]
fn select_director_tab(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    select_director_tab_outcome(key, ui, runtime) != DirectorTabSelection::Unhandled
}

fn select_director_tab_and_activate(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) -> bool {
    let outcome = select_director_tab_outcome(key, ui, runtime);
    if outcome == DirectorTabSelection::Selected
        && matches!(runtime.state().director_route(), DirectorRoute::Console(_))
    {
        activate_focused_interrupted_tab(ui, runtime, pending_targets);
    }
    outcome != DirectorTabSelection::Unhandled
}

fn select_root_terminal_tab(key: &Key, runtime: &mut WorkspaceRuntime) -> bool {
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

/// Select one visible managed-session tab after the frame's hit test resolved
/// its display index. Agent selection is committed before registry mutation,
/// matching keyboard tab cycling's durability fence.
/// Focus the Agent tab of the rabbit a Garden click landed on.
///
/// The Garden itself owns no target semantics beyond the session activation the
/// reducer already performed ([`GardenClick`]); this only moves the selection
/// inside the Closeup that activation opened, through the same stable-identity
/// path a click on the tab strip uses.
fn visit_garden_agent(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    click: GardenClick,
) -> bool {
    let GardenClick::Visit {
        agent: Some(runtime_id),
        ..
    } = click
    else {
        return false;
    };
    if !runtime.wants_right_pane_tab_click() {
        return false;
    }
    let Some(index) = runtime.agent_tab_index(runtime_id, ui.agent_inventory()) else {
        return false;
    };
    select_right_pane_tab(ui, runtime, index)
}

fn select_right_pane_tab(
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

fn is_director_new_click(
    key: &Key,
    runtime: &WorkspaceRuntime,
    height: usize,
    width: usize,
) -> bool {
    let (column, row) = match key {
        Key::Click { column, row }
        | Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => (*column, *row),
        _ => return false,
    };
    runtime.state().overlay().is_none()
        && runtime.state().director_drawer_open()
        && runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Director)
        && matches!(runtime.state().director_new(), DirectorNew::Idle)
        && runtime.state().director_launching().is_none()
        && director_drawer::new_button_at(
            height,
            width,
            column,
            row,
            runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven,
            false,
        )
}

/// Resolve the Director's visible New / Start button before route-local input.
///
/// Work Runs deliberately owns otherwise-unrecognized clicks, so deferring
/// this chrome action would make the primary Goal-driven CTA inert.
fn open_director_from_new_button(
    runtime: &mut WorkspaceRuntime,
    key: &Key,
    height: usize,
    width: usize,
    work_run_mode: WorkRunControlMode,
) -> Option<Vec<Effect>> {
    (work_run_mode != WorkRunControlMode::Submitting
        && is_director_new_click(key, runtime, height, width))
    .then(|| runtime.apply_event(AppEvent::Key(AppKey::OpenDirectorNew)))
}

/// Resolve a persistent workspace-drawer button in the Home header that
/// produced the current frame.
///
/// This is resolved before drawer-local input ownership. In particular, the
/// Director picker deliberately consumes every other user input, but must not
/// make either visible drawer button inert.
fn workspace_drawer_header_key(key: &Key, width: usize, home: &HomeProjection) -> Option<AppKey> {
    let (column, row) = match key {
        Key::Click { column, row }
        | Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => (*column, *row),
        _ => return None,
    };
    match home_header_action_at(width, home, column, row) {
        Some(HomeHeaderAction::Director) => Some(AppKey::ToggleDirectorDrawer),
        Some(HomeHeaderAction::RootTerminal) => Some(AppKey::ToggleRootTerminalDrawer),
        _ => None,
    }
}

/// Apply a workspace-drawer header button while Director is open, before its
/// exclusive picker consumes the press. Existing modals keep precedence, and a
/// closed Director continues through the ordinary Home header route.
fn apply_drawer_header_while_director_open(
    runtime: &mut WorkspaceRuntime,
    key: &Key,
    width: usize,
    home: &HomeProjection,
) -> Option<Vec<Effect>> {
    if runtime.state().overlay().is_some() || !runtime.state().director_drawer_open() {
        return None;
    }
    let key = workspace_drawer_header_key(key, width, home)?;
    Some(runtime.apply_event(AppEvent::Key(key)))
}

fn is_director_new_pointer(
    key: &Key,
    runtime: &WorkspaceRuntime,
    height: usize,
    width: usize,
) -> bool {
    let (column, row) = match key {
        Key::Click { column, row } | Key::Pointer(PointerEvent { column, row, .. }) => {
            (*column, *row)
        }
        _ => return false,
    };
    runtime.state().director_drawer_open()
        && director_drawer::new_button_at(
            height,
            width,
            column,
            row,
            runtime.state().work_mode() == usagi_core::domain::settings::WorkMode::GoalDriven,
            false,
        )
}

/// Focus the visible workspace drawer under a pointer press. Director is
/// tested first because it is composed last; a Shell remains clickable only in
/// the portion the right-side overlay does not cover.
fn focus_workspace_drawer_from_pointer(
    runtime: &mut WorkspaceRuntime,
    key: &Key,
    height: usize,
    width: usize,
) -> Option<WorkspaceDrawerFocus> {
    if runtime.state().overlay().is_some() {
        return None;
    }
    let (column, row) = match key {
        Key::Click { column, row }
        | Key::Pointer(PointerEvent {
            kind: PointerKind::Down,
            column,
            row,
        }) => (usize::from(*column), usize::from(*row)),
        _ => return None,
    };
    let director = runtime.state().director_drawer_open().then(|| {
        let geometry = director_drawer::geometry(height, width);
        (geometry.left..geometry.left.saturating_add(geometry.width)).contains(&column)
            && (geometry.top..geometry.top.saturating_add(geometry.height)).contains(&row)
    });
    let focus = if director == Some(true) {
        Some(WorkspaceDrawerFocus::Director)
    } else if runtime.state().root_terminal_drawer_open() {
        let geometry = root_terminal_drawer::geometry_for_mode(
            height,
            width,
            workspace::root_terminal_available_width(
                height,
                width,
                runtime.state().director_drawer_open(),
            ),
            runtime.state().root_terminal_full_height(),
        );
        ((geometry.left..geometry.left.saturating_add(geometry.width)).contains(&column)
            && (geometry.top..geometry.top.saturating_add(geometry.height)).contains(&row))
        .then_some(WorkspaceDrawerFocus::Terminal)
    } else {
        None
    }?;
    let _ = runtime.apply_event(AppEvent::WorkspaceDrawerFocused(focus));
    Some(focus)
}

/// Intercept the live-terminal view controls the Home reducer does not own —
/// copy, scroll, tab close, and pointer drag — returning `true` when the key was
/// consumed here so the shell loop skips reducer dispatch. `rows_len` / `scroll`
/// describe the frame's projected viewport for pointer mapping.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn intercept_live_terminal_control(
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
        if let Some(index) = root_terminal_drawer::tab_at_for_mode(
            height,
            width,
            workspace::root_terminal_available_width(
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
                    director_drawer::terminal_point_at(height, width, 0, 0, *column, *row)
                } else if runtime.state().workspace_drawer_focus()
                    == Some(WorkspaceDrawerFocus::Terminal)
                {
                    root_terminal_drawer::terminal_point_at_for_mode(
                        height,
                        width,
                        workspace::root_terminal_available_width(
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

/// Everything the Home frame is a function of.
///
/// [`render_home_material`] is pure in this value, so the shell can compare it
/// against the material it last drew and skip both the frame build and
/// [`Terminal::draw`] when nothing changed (#554). Comparing the renderer's
/// inputs is what makes the skip safe, and it holds only because the renderer
/// reads nothing else — [`render_home_at`] takes even the wall clock as an
/// argument for that reason. A new renderer input belongs here too.
#[derive(Debug, PartialEq, Eq)]
struct HomeFrameMaterial {
    height: usize,
    width: usize,
    projection: HomeProjection,
    /// Safe label/reason and focused Remove/Keep answer for an unresumable
    /// interrupted conversation explicitly selected by the user.
    interrupted_removal_confirmation: Option<(String, String, bool)>,
    /// `Some(choice)` exactly while the exit prompt covers the frame, carrying
    /// the answer its focused button would commit (#556).
    quit_confirmation: Option<ExitChoice>,
    /// The create-failure dialog's safe message, present exactly while its
    /// overlay is open. Keying off the message avoids an unreachable "error
    /// overlay without a message" branch.
    create_error: Option<String>,
    /// The terminal-launch failure dialog's safe message.
    terminal_launch_error: Option<String>,
    /// The Agent-launch failure dialog's safe message.
    agent_launch_error: Option<String>,
    /// Failed-delete session label and focused Yes/No answer.
    force_remove_confirmation: Option<(String, bool)>,
    environment_editor: Option<crate::usecase::application::controller::EnvironmentEditor>,
    role_editor: Option<crate::usecase::application::controller::RoleEditor>,
    /// Minute-resolution wall clock behind relative session labels. Garden
    /// animation has a separate monotonic logical clock and never reads this.
    now: DateTime<Utc>,
}

/// Cheap dependency vector for the owned Home projection. Equality is the
/// admission gate to projection construction; each revision is advanced by its
/// authoritative controller/daemon source, never by this cache.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FrameMaterialKey {
    height: usize,
    width: usize,
    controller: (u64, u64),
    sessions: (u64, Option<SessionId>, u64),
    shell: u64,
    metrics: u64,
    terminal: (u64, u64, u64, u64),
    animation: u64,
    create_pending: Option<String>,
    /// Rounds of the Garden's cross-project observation that changed a plot.
    /// The other projects' rabbits are draw material this loop owns, so their
    /// change has to reach the key that admits a rebuild.
    garden_observations: u64,
    work_run_revision: u64,
    now: DateTime<Utc>,
}

impl FrameMaterialKey {
    /// Only controller admission can conservatively advance for a reducer no-op
    /// (for example Escape on the base route). Every other generation denotes
    /// changed draw material and can bypass an owned-projection equality scan.
    fn differs_only_by_controller(&self, other: &Self) -> bool {
        self.controller != other.controller
            && self.height == other.height
            && self.width == other.width
            && self.sessions == other.sessions
            && self.shell == other.shell
            && self.metrics == other.metrics
            && self.terminal == other.terminal
            && self.animation == other.animation
            && self.create_pending == other.create_pending
            && self.garden_observations == other.garden_observations
            && self.work_run_revision == other.work_run_revision
            && self.now == other.now
    }
}

impl HomeFrameMaterial {
    fn with_agent_inventory(
        mut self,
        inventory: Option<&AgentInventory>,
        panes: &PaneRegistry,
    ) -> Self {
        self.projection = self
            .projection
            .with_agent_inventory_and_panes(inventory, panes);
        self
    }

    fn with_work_runs(mut self, runs: WorkRunProjection) -> Self {
        self.projection = self.projection.with_work_runs(runs);
        self
    }

    fn with_garden_animation(mut self, tick: u64, reduced_motion: bool) -> Self {
        self.projection = self.projection.with_garden_reduced_motion(reduced_motion);
        self.projection = self
            .projection
            .with_garden_tick(self.height, self.width, tick);
        self
    }

    fn with_workspace_deck_garden(mut self, deck: &WorkspaceDeck) -> Self {
        let Some(active_sessions) = self.projection.garden_sessions().map(<[_]>::to_vec) else {
            return self;
        };
        let (scope, sessions) = deck.garden_projection(&active_sessions);
        self.projection = self.projection.with_deck_garden(scope, sessions);
        self
    }
}

#[allow(clippy::too_many_arguments)]
fn home_frame_material(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    sessions: &[ProjectedSession],
    metrics: Option<usagi_core::infrastructure::client::DaemonMetrics>,
    health: crate::usecase::application::daemon_health::DaemonHealthTracker,
    git_diffs: &BTreeMap<SessionId, GitDiff>,
    terminal_view: Option<TerminalViewProjection>,
    create_pending: Option<&str>,
    now: DateTime<Utc>,
) -> HomeFrameMaterial {
    let (managed_terminal_view, root_terminal_view) =
        if runtime.state().workspace_drawer_focus() == Some(WorkspaceDrawerFocus::Terminal) {
            (None, terminal_view.map(Arc::new))
        } else {
            (terminal_view.map(Arc::new), None)
        };
    home_frame_material_shared(
        height,
        width,
        runtime,
        workspace_name,
        Arc::from(sessions.to_vec()),
        metrics,
        health,
        Arc::new(git_diffs.clone()),
        managed_terminal_view,
        root_terminal_view.as_deref(),
        create_pending,
        now,
        usagi_core::domain::settings::IconMode::default(),
    )
    .with_garden_animation(
        widgets::garden::runtime_tick(runtime.state().mascot_tick()),
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn home_frame_material_shared(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    sessions: Arc<[ProjectedSession]>,
    metrics: Option<usagi_core::infrastructure::client::DaemonMetrics>,
    health: crate::usecase::application::daemon_health::DaemonHealthTracker,
    git_diffs: Arc<BTreeMap<SessionId, GitDiff>>,
    managed_terminal_view: Option<Arc<TerminalViewProjection>>,
    root_terminal_view: Option<&TerminalViewProjection>,
    create_pending: Option<&str>,
    now: DateTime<Utc>,
    icon_mode: usagi_core::domain::settings::IconMode,
) -> HomeFrameMaterial {
    let force_remove_confirmation =
        runtime
            .state()
            .force_remove_confirmation()
            .and_then(|(target, confirm)| {
                sessions
                    .iter()
                    .find(|session| session.id == target)
                    .map(|session| (session.label.clone(), confirm))
            });
    let root_terminal_projection = runtime.root_terminal_projection(root_terminal_view);
    let projection = HomeProjection::from_ordered_state(runtime.state(), workspace_name, sessions)
        .with_icon_mode(icon_mode)
        .with_pane(runtime.preview_pane())
        .with_metrics(metrics)
        // Diagnostic-only material. It rides the frame material like every
        // other renderer input, so an idle Home still skips redraws.
        .with_health(health)
        .with_shared_git_diffs(git_diffs)
        .with_shared_terminal_view(managed_terminal_view)
        .with_director_drawer(runtime.director_projection().clone())
        .with_root_terminal_drawer(root_terminal_projection)
        .with_create_pending(create_pending.map(str::to_owned))
        .with_overlay_modals(
            runtime.overview_modal().cloned(),
            runtime.closeup_modal().cloned(),
        )
        // Last, once every surface that reads the animation clock is known.
        .collapse_animation_clock();
    HomeFrameMaterial {
        height,
        width,
        projection,
        interrupted_removal_confirmation: runtime.interrupted_removal_confirmation().map(
            |confirmation| {
                (
                    confirmation.tab().safe_label(),
                    confirmation.tab().safe_detail().to_owned(),
                    confirmation.is_confirm_selected(),
                )
            },
        ),
        quit_confirmation: (runtime.state().overlay() == Some(Overlay::QuitConfirmation))
            .then(|| runtime.state().exit_choice()),
        create_error: runtime
            .state()
            .create_session_error()
            .map(|error| error.message.clone()),
        terminal_launch_error: runtime
            .state()
            .terminal_launch_error()
            .map(|error| error.message.clone()),
        agent_launch_error: runtime
            .state()
            .agent_launch_error()
            .map(|error| error.message.clone()),
        force_remove_confirmation,
        environment_editor: runtime.state().environment_editor().cloned(),
        role_editor: runtime.state().role_editor().cloned(),
        // Garden canonicalization happens only after every composition-owned
        // source (notably Agent inventory and reduced motion) is attached.
        now: relative_time_clock(now),
    }
}

fn relative_time_clock(now: DateTime<Utc>) -> DateTime<Utc> {
    now.with_second(0)
        .and_then(|now| now.with_nanosecond(0))
        .unwrap_or(now)
}

/// Compose the controller Home frame: [`render_home_at`] plus the shell
/// overlays it does not own (quit confirmation, create-failure dialog).
fn render_home_material(material: &HomeFrameMaterial) -> Vec<String> {
    let frame = render_home_at(
        material.height,
        material.width,
        &material.projection,
        material.now,
    );
    // The create form renders inline in the `+ new session` sidebar row (see
    // `render_home`), so no overlay composite is needed here.
    if let Some((label, message, confirm)) = &material.interrupted_removal_confirmation {
        let title = Style::new()
            .fg(Color::White)
            .bold()
            .paint("Remove interrupted Agent");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint(&format!("Remove {label}?"));
        let mut view = ConfirmationView::confirmation(&title, 60, heading, message);
        view.confirm_label = "remove";
        view.cancel_label = "keep";
        view.hints = "Enter: select   y: remove   Esc/n: keep   ←→/Tab: move";
        return modal::render_confirmation_over(
            material.height,
            material.width,
            &frame,
            modal::ConfirmationModal::from_confirm_selected(*confirm),
            view,
        );
    }
    if let Some(choice) = material.quit_confirmation {
        return quit_modal::render_over(material.height, material.width, &frame, choice);
    }
    if let Some(message) = &material.create_error {
        return create_session_error_modal::render_over(
            material.height,
            material.width,
            &frame,
            message,
        );
    }
    if let Some(message) = &material.terminal_launch_error {
        return create_session_error_modal::render_titled_over(
            material.height,
            material.width,
            &frame,
            "Terminal failed to open",
            message,
        );
    }
    if let Some(message) = &material.agent_launch_error {
        return create_session_error_modal::render_titled_over(
            material.height,
            material.width,
            &frame,
            "Agent failed to start",
            message,
        );
    }
    if let Some((label, confirm)) = &material.force_remove_confirmation {
        let title = Style::new().fg(Color::White).bold().paint("Force remove");
        let heading = Style::new()
            .fg(Color::White)
            .bold()
            .paint(&format!("Force remove {label}?"));
        return modal::render_confirmation_over(
            material.height,
            material.width,
            &frame,
            modal::ConfirmationModal::from_confirm_selected(*confirm),
            ConfirmationView::confirmation(
                &title,
                52,
                heading,
                "Previous removal failed. Changes may be discarded.",
            ),
        );
    }
    if let Some(editor) = &material.environment_editor {
        return scratchpad_modal::render_environment_over(
            material.height,
            material.width,
            &frame,
            editor,
        );
    }
    if let Some(editor) = &material.role_editor {
        let height = material.height;
        let width = material.width;
        return scratchpad_modal::render_roles_over(height, width, &frame, editor);
    }
    frame
}

#[allow(clippy::too_many_arguments)]
fn render_controller_frame(
    height: usize,
    width: usize,
    runtime: &WorkspaceRuntime,
    workspace_name: &str,
    sessions: &[ProjectedSession],
    metrics: Option<usagi_core::infrastructure::client::DaemonMetrics>,
    health: crate::usecase::application::daemon_health::DaemonHealthTracker,
    git_diffs: &BTreeMap<SessionId, GitDiff>,
    terminal_view: Option<TerminalViewProjection>,
    create_pending: Option<&str>,
) -> Vec<String> {
    render_home_material(&home_frame_material(
        height,
        width,
        runtime,
        workspace_name,
        sessions,
        metrics,
        health,
        git_diffs,
        terminal_view,
        create_pending,
        Utc::now(),
    ))
}

/// Add the project tab bar and deck overlay to a Home-height frame.
///
/// Both the ordinary frame loop and modal backgrounds use this composition so
/// opening a modal cannot move the workspace projection into row zero.
fn compose_workspace_shell_frame(
    deck: &WorkspaceDeck,
    home_height: usize,
    width: usize,
    home: &[String],
) -> Vec<String> {
    let mut frame = Vec::with_capacity(home_height.saturating_add(PROJECT_BAR_ROWS));
    frame.push(project_bar(deck, width).line);
    frame.extend(render_overlay(deck, home_height, width, home));
    frame
}

/// Apply actions already routed by [`DaemonBackend`] to the stateful terminal
/// host. This layer owns no Effect matching and therefore cannot diverge from
/// the backend's route matrix.
#[allow(clippy::too_many_lines)]
fn drain_controller_host_actions(
    actions: &Receiver<ControllerHostAction>,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    session_refresh: &mut dyn SessionRefreshPort,
    pending_session_refresh: &mut Option<Completions>,
) {
    for action in actions.try_iter().take(FRAME_EVENT_BUDGET) {
        match action {
            ControllerHostAction::Create(request, completions) => {
                let name = request.intent.name;
                let base_ref = request.intent.base_ref;
                let role_id = request.intent.role_id;
                let before = ui.workspace.session_ids().to_vec();
                if begin_session_command(
                    ui,
                    SessionCommand::Create {
                        name: name.clone(),
                        role_id,
                        base_ref,
                    },
                    SessionBackendCompletion::Create {
                        token: request.token,
                        before,
                        completions,
                    },
                ) {
                    ui.creating_session = Some(PendingCreate { name });
                }
            }
            ControllerHostAction::Refresh(_, completions) => {
                // A refresh is an observation, not a command: it goes to the
                // resident lane instead of spawning a worker with its own
                // daemon connection (#551). Several requests inside one cadence
                // period coalesce onto the snapshot that lane publishes next.
                session_refresh.wake();
                *pending_session_refresh = Some(completions);
            }
            ControllerHostAction::Remove(request, completions) => {
                if let Some(name) = session_name_for(ui, request.session) {
                    let before = ui.workspace.session_ids().to_vec();
                    if begin_session_command(
                        ui,
                        SessionCommand::Remove {
                            name,
                            force: request.force,
                            force_delete_branch: request.force_delete_branch,
                            purge_orphan: request.purge_orphan,
                        },
                        SessionBackendCompletion::Remove {
                            session: request.session,
                            before,
                            completions,
                        },
                    ) {
                        ui.removing_session = Some(request.session);
                    }
                } else {
                    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                        "selected session is no longer available",
                    ))));
                }
            }
            ControllerHostAction::Sleep(request, completions) => {
                if let Some(name) = session_name_for(ui, request.session) {
                    let before = ui.workspace.session_ids().to_vec();
                    begin_session_command(
                        ui,
                        SessionCommand::Sleep { name },
                        SessionBackendCompletion::Sleep {
                            before,
                            completions,
                        },
                    );
                } else {
                    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                        "selected session is no longer available",
                    ))));
                }
            }
            ControllerHostAction::LaunchAgent(request) => {
                let target = request
                    .session
                    .map_or(Target::Root(request.workspace), Target::Session);
                pending_targets.insert(request.operation_id, target);
                if let Some(goal) = &request.goal {
                    runtime.on_effect(&Effect::LaunchGoal {
                        workspace: request.workspace,
                        operation_id: request.operation_id,
                        profile: request.profile.clone(),
                        goal: goal.clone(),
                    });
                } else {
                    runtime.on_effect(&Effect::LaunchAgent {
                        workspace: request.workspace,
                        session: request.session,
                        operation_id: request.operation_id,
                        profile: request.profile.clone(),
                    });
                }
                enqueue_pane_launch(
                    ui,
                    PaneLaunch::Agent {
                        operation: request.operation_id,
                        workspace: request.workspace,
                        session: request.session,
                        profile: request.profile,
                        goal: request.goal,
                        resume: false,
                    },
                );
            }
            ControllerHostAction::ResumeAgent(request) => {
                let target = Target::Session(request.session);
                pending_targets.insert(request.operation_id, target);
                runtime.on_effect(&Effect::LaunchAgent {
                    workspace: request.workspace,
                    session: Some(request.session),
                    operation_id: request.operation_id,
                    profile: None,
                });
                enqueue_pane_launch(
                    ui,
                    PaneLaunch::Agent {
                        operation: request.operation_id,
                        workspace: request.workspace,
                        session: Some(request.session),
                        profile: None,
                        goal: None,
                        resume: true,
                    },
                );
            }
            ControllerHostAction::ReopenAgent(request) => {
                if ui
                    .agent
                    .as_ref()
                    .is_some_and(|agent| request.workspace == agent.workspace)
                {
                    let reopened = ui.mutate_agent_intent(AgentTabIntentMutation::Reopen {
                        continuation: request.continuation,
                    });
                    match reopened {
                        Ok(()) => {
                            ui.request_agent_observation();
                            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(
                                Notice::new(
                                    "Agent reopen was saved; waiting for daemon observation",
                                ),
                            )));
                        }
                        Err(error) => surface_agent_tab_intent_error(runtime, error),
                    }
                }
            }
            ControllerHostAction::OpenTerminal(request) => {
                if let Some(agent) = ui.agent.as_ref() {
                    let workspace = agent.workspace;
                    pending_targets.insert(request.operation_id, request.target);
                    runtime.on_effect(&Effect::OpenTerminal {
                        target: request.target,
                        operation_id: request.operation_id,
                        arguments: request.arguments.clone(),
                    });
                    enqueue_pane_launch(
                        ui,
                        PaneLaunch::Terminal {
                            operation: request.operation_id,
                            workspace,
                            session: request.target.session_id(),
                            arguments: request.arguments,
                        },
                    );
                } else {
                    runtime.on_effect(&Effect::OpenTerminal {
                        target: request.target,
                        operation_id: request.operation_id,
                        arguments: request.arguments,
                    });
                    fail_terminal_launch(
                        runtime,
                        request.target,
                        request.operation_id,
                        "terminal launch is unavailable".to_owned(),
                    );
                }
            }
            ControllerHostAction::OpenExternalTerminal(target) => {
                let path = match target {
                    Target::Root(_) => Some(ui.workspace.path().to_path_buf()),
                    Target::Session(session) => ui
                        .workspace
                        .sessions()
                        .iter()
                        .zip(ui.workspace.session_ids())
                        .find(|(_, id)| **id == session)
                        .map(|(record, _)| record.root.clone()),
                };
                match path {
                    Some(path) => {
                        if let Err(error) = ui.external_terminal.open(&path) {
                            let _ = runtime
                                .apply_event(AppEvent::TerminalLaunchFailed(Notice::new(error)));
                        }
                    }
                    None => {
                        let _ = runtime.apply_event(AppEvent::TerminalLaunchFailed(Notice::new(
                            "selected session is no longer available",
                        )));
                    }
                }
            }
            ControllerHostAction::SelectTab(direction) => {
                let Some(active) = runtime.panes().active() else {
                    continue;
                };
                let Some(selection) = runtime.selection_after_select(direction) else {
                    continue;
                };
                if !ui.has_agent_intent_for(active.session_id()) {
                    runtime.on_effect(&Effect::SelectTab { direction });
                    activate_focused_interrupted_tab(ui, runtime, pending_targets);
                    continue;
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
                        runtime.on_effect(&Effect::SelectTab { direction });
                        activate_focused_interrupted_tab(ui, runtime, pending_targets);
                    }
                    Err(error) => surface_agent_tab_intent_error(runtime, error),
                }
            }
        }
    }
}

/// Apply completed pane launches: promote and focus the runtime tab, then attach
/// the daemon terminal stream, so the live viewport renders next frame.
///
/// A completion frees the launch admission slot only when its fence matches the
/// admitted worker, so a duplicate, late, or unadmitted (Busy) completion cannot
/// release a newer worker's slot. Which pending pane it applies to remains fenced
/// by `pending_targets` and the runtime's own operation identity.
fn drain_pane_completions_into_runtime(
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

fn complete_terminal_launch(
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
fn fail_terminal_launch(
    runtime: &mut WorkspaceRuntime,
    target: Target,
    operation: OperationId,
    message: String,
) {
    let _ = runtime.fail_pane(target, operation, message.clone());
    let _ = runtime.apply_event(AppEvent::TerminalLaunchFailed(Notice::new(message)));
}

fn apply_agent_launch_completion(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
    operation: OperationId,
    result: Result<AgentPaneAdmission, String>,
) {
    if result.is_ok() {
        // A cached pre-launch inventory must not filter an admitted runtime out.
        ui.request_agent_inventory_change_observation();
    }
    let Some(target) = pending_targets.remove(&operation) else {
        return;
    };
    let admission = match result {
        Ok(admission) => admission,
        Err(message) => {
            let _ = runtime.fail_pane(target, operation, message.clone());
            let _ = runtime.apply_event(AppEvent::AgentLaunchFailed(Notice::new(message)));
            complete_director_launch(runtime, target, operation, None, false);
            return;
        }
    };
    let supervisor_run_id = admission.supervisor_run_id;
    let terminal = admission.terminal;
    if let Some(continuation) = admission.continuation {
        let select =
            matches!(target, Target::Root(_)) || runtime.pane_completion_will_focus(operation);
        match ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
            session_id: target.session_id(),
            continuation,
            terminal: terminal.clone(),
            select,
        }) {
            Ok(()) => {
                let _ = runtime.complete_pane_focus_if_uninterrupted(target, operation, terminal);
            }
            Err(error) => {
                let _ = runtime.fail_pane(target, operation, error.safe_message().to_owned());
                surface_agent_tab_intent_error(runtime, error);
            }
        }
    } else if matches!(target, Target::Root(_)) && ui.agent_tab_intent.is_some() {
        let _ = runtime.fail_pane(
            target,
            operation,
            "daemon did not return a root Agent conversation".to_owned(),
        );
    } else {
        let _ = runtime.complete_pane_focus_if_uninterrupted(target, operation, terminal);
    }
    complete_director_launch(runtime, target, operation, supervisor_run_id, true);
}

fn complete_director_launch(
    runtime: &mut WorkspaceRuntime,
    target: Target,
    operation: OperationId,
    supervisor_run_id: Option<SupervisorRunId>,
    succeeded: bool,
) {
    if matches!(target, Target::Root(_)) {
        let _ = runtime.apply_event(AppEvent::DirectorLaunchFinished {
            operation,
            supervisor_run_id,
            succeeded,
        });
    }
}

/// Apply one explicit per-tab resume answer (#510).
///
/// The runtime validates the answer against the exact interrupted tab before any
/// tab changes; only an accepted replacement turns that one tab live, and its new
/// terminal is recorded as #506 display intent so the next observation keeps it.
/// Every refusal leaves the interrupted tab in place with safe feedback.
fn apply_exact_resume(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    target: Target,
    operation: OperationId,
    continuation: AgentContinuationRef,
    result: Result<ExactAgentResume, String>,
) {
    let resume = match result {
        Ok(resume) => resume,
        Err(message) => {
            runtime.fail_tab_resume_for(target, continuation, Some(operation), message);
            return;
        }
    };
    if let Err(rejection) = runtime.validate_tab_resume_for(
        target,
        continuation,
        operation,
        resume.continuation,
        resume.relation.as_ref(),
        &resume.terminal,
    ) {
        runtime.fail_tab_resume_for(
            target,
            continuation,
            Some(operation),
            rejection.safe_message().to_owned(),
        );
        return;
    }
    let session_id = target.session_id();
    if let Err(error) = ui.mutate_agent_intent(AgentTabIntentMutation::Upsert {
        session_id,
        continuation,
        terminal: resume.terminal.clone(),
        select: false,
    }) {
        runtime.fail_tab_resume_for(
            target,
            continuation,
            Some(operation),
            error.safe_message().to_owned(),
        );
        surface_agent_tab_intent_error(runtime, error);
        return;
    }
    let accepted = runtime.complete_tab_resume_for(
        target,
        continuation,
        operation,
        resume.continuation,
        resume.relation.as_ref(),
        &resume.terminal,
    );
    debug_assert!(accepted.is_ok(), "validated exact resume remains accepted");
}

/// Activate an interrupted tab after an explicit user selection. Passive
/// inventory restoration never calls this: a resumable lineage starts its
/// exact resume, while an unresumable lineage opens a destructive prompt.
fn activate_focused_interrupted_tab(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    if runtime.state().overlay().is_some() {
        return;
    }
    let Some(resumable) = runtime.focused_interrupted().map(InterruptedTab::resumable) else {
        return;
    };
    if resumable {
        resume_focused_interrupted_tab(ui, runtime, pending_targets);
    } else {
        let _ = runtime.open_interrupted_removal_confirmation();
    }
}

/// Give the unresumable-history prompt exclusive ownership of one key.
/// Persistence is committed before the exact tab leaves the registry; a failed
/// commit closes the prompt but leaves the history visible with a safe notice.
fn handle_interrupted_removal_confirmation(
    key: &Key,
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
) -> bool {
    let Some(confirm_selected) = runtime
        .interrupted_removal_confirmation()
        .map(InterruptedRemovalConfirmation::is_confirm_selected)
    else {
        return false;
    };
    match key {
        Key::Left | Key::Right | Key::Tab => {
            runtime.toggle_interrupted_removal_choice();
        }
        Key::Char('y' | 'Y') => {
            confirm_interrupted_removal(ui, runtime);
        }
        Key::Enter => {
            if confirm_selected {
                confirm_interrupted_removal(ui, runtime);
            } else {
                let _ = runtime.take_interrupted_removal_confirmation();
            }
        }
        Key::Escape | Key::Char('n' | 'N') => {
            let _ = runtime.take_interrupted_removal_confirmation();
        }
        _ => {}
    }
    true
}

fn confirm_interrupted_removal(ui: &mut WorkspaceIoRuntime, runtime: &mut WorkspaceRuntime) {
    let Some(confirmation) = runtime.take_interrupted_removal_confirmation() else {
        return;
    };
    dismiss_interrupted_history(
        ui,
        runtime,
        confirmation.target(),
        confirmation.tab().clone(),
    );
}

/// Start an explicit resume of the selected interrupted tab (`Ctrl-O r` or a
/// direct tab selection).
///
/// This is the only path that asks the daemon to resume a provider conversation
/// per tab: the request carries the daemon's own opaque target plus a fresh
/// durable operation, and it marks exactly that tab pending.
fn resume_focused_interrupted_tab(
    ui: &mut WorkspaceIoRuntime,
    runtime: &mut WorkspaceRuntime,
    pending_targets: &mut std::collections::HashMap<OperationId, Target>,
) {
    let Some(workspace) = ui.agent.as_ref().map(|agent| agent.workspace) else {
        return;
    };
    let Some(target) = runtime.panes().active() else {
        return;
    };
    let Some(continuation) = runtime
        .focused_interrupted()
        .map(|interrupted| interrupted.continuation)
    else {
        return;
    };
    // A refusal (no trustworthy exact target, or a repeated activation whose
    // request is already in flight) is already the pane's own feedback and must
    // never reach the daemon as a second request.
    let Ok(ResumeCommand {
        target: resume_target,
        operation,
    }) = runtime.resume_selected_tab(OperationId::new())
    else {
        return;
    };
    debug_assert_eq!(resume_target.workspace_id, workspace);
    pending_targets.insert(operation, target);
    enqueue_pane_launch(
        ui,
        PaneLaunch::ResumeExact {
            operation,
            continuation,
            target: resume_target,
        },
    );
}

/// Build the controller event for a sidebar click. The shell supplies the raw
/// cell and an injected monotonic timestamp; stable identity and double-click
/// detection remain controller responsibilities.
fn sidebar_pointer_event(column: u16, row: u16, at: std::time::Duration) -> AppEvent {
    AppEvent::Pointer { column, row, at }
}

/// Whether a key read from the terminal is a *user* interaction.
///
/// [`Key::Other`] is how the composition root delivers a frame wake-up: an
/// animation tick, a drained daemon event, or terminal output arriving behind
/// the frame. None of those is a person touching the terminal, so none of them
/// postpones the screen saver — an Agent can work for an hour and the garden
/// still opens. Everything else in the vocabulary — keys, paste, pointer
/// presses and wheel, the OS copy shortcut, PTY passthrough, and a resize —
/// is an interaction and resets the idle clock.
const fn is_user_activity(key: &Key) -> bool {
    !matches!(key, Key::Other)
}

/// Workspace-local drafts are not deck state and must not be discarded by a
/// project activation.
fn workspace_has_unsaved_surface(runtime: &WorkspaceRuntime) -> bool {
    let state = runtime.state();
    state.create_session_form().is_some()
        || state.note_editor().is_some()
        || state.environment_editor().is_some()
        || state.role_editor().is_some()
}

/// Carry the active workspace's sidebar cursor across the composition teardown.
/// Only stable session rows are remembered; transient action rows remain local
/// to the controller that owns them.
fn remember_workspace_session_focus(deck: &mut WorkspaceDeck, state: &AppState) {
    if let crate::usecase::application::controller::Selection::Target(Target::Session(session)) =
        state.selected()
    {
        deck.remember_session_focus(state.workspace(), session);
    }
}

fn restore_workspace_session_focus(
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
fn restore_workspace_closeup(
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

/// Run one blocking operation on a scoped worker while continuing to paint.
/// For cancellable workspace opens, Escape discards a late result; the
/// underlying call is allowed to settle safely before its borrowed port is
/// returned. Non-cancellable saves leave queued input untouched.
fn run_workspace_loading<T: Send>(
    term: &mut dyn Terminal,
    label: &str,
    cancellable: bool,
    operation: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    run_workspace_loading_with(
        term,
        label,
        cancellable,
        false,
        operation,
        |height, width, frame, status, _| {
            widgets::loading::loading_screen(width, height, frame, frame / 3, status)
        },
    )
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=blocking_operations_keep_painting_and_workspace_open_can_be_cancelled
fn run_workspace_loading_with<T: Send>(
    term: &mut dyn Terminal,
    label: &str,
    cancellable: bool,
    defer_progress: bool,
    operation: impl FnOnce() -> io::Result<T> + Send,
    mut render: impl FnMut(usize, usize, usize, &str, bool) -> Vec<String>,
) -> io::Result<T> {
    std::thread::scope(|scope| {
        let worker = scope.spawn(operation);
        let mut frame = 0_usize;
        let mut cancelled = false;
        let started = std::time::Instant::now();
        loop {
            let (height, width) = term.size()?;
            let status = if cancelled { "Cancelling…" } else { label };
            let show_progress =
                workspace_loading_visible(defer_progress, cancelled, started.elapsed());
            term.draw(&render(height, width, frame, status, show_progress))?;
            // Even an immediately completed operation gets one visible frame.
            // Without this fence, fast settings writes skipped feedback
            // entirely and made the Enter key appear to do nothing.
            if worker.is_finished() {
                break;
            }
            let tick = if defer_progress && !show_progress {
                std::time::Duration::from_millis(16)
            } else {
                std::time::Duration::from_millis(80)
            };
            if cancellable && !cancelled {
                if let Some(key) = term.wait_for_key(tick)? {
                    if matches!(key, Key::Escape) {
                        cancelled = true;
                    } else {
                        term.defer_key(key);
                    }
                }
            } else {
                // Saving and the post-cancel wait cannot accept input, but they
                // must still yield so a slow operation does not become a render
                // loop or consume keys intended for the next screen.
                term.wait(tick)?;
            }
            frame = frame.wrapping_add(1);
        }
        let result = worker
            .join()
            .map_err(|_| io::Error::other("background operation stopped unexpectedly"))?;
        if cancelled {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "operation was cancelled",
            ))
        } else {
            result
        }
    })
}

fn workspace_loading_visible(
    defer_progress: bool,
    cancelled: bool,
    elapsed: std::time::Duration,
) -> bool {
    cancelled || !defer_progress || elapsed >= WORKSPACE_SWITCH_LOADING_GRACE
}

fn cached_workspace_switch_frame(
    deck: &WorkspaceDeck,
    path: &Path,
    height: usize,
    width: usize,
    frame: usize,
    status: &str,
    show_progress: bool,
) -> Option<Vec<String>> {
    let slot = deck.slot_for_path(path)?;
    let sessions = slot.projected_sessions();
    let mut state = AppState::home(
        slot.workspace_id(),
        sessions.iter().map(|session| session.id).collect(),
    );
    if let Some(session) = deck.focused_session_for_path(path) {
        let _ = crate::usecase::application::controller::update(
            &mut state,
            AppEvent::FocusSession(session),
        );
    }
    let projection = HomeProjection::from_state(&state, slot.label(), &sessions)
        .with_icon_mode(deck.icon_mode());
    let projection = if show_progress {
        projection.with_content_loading(status, frame)
    } else {
        projection.with_content_pending()
    };
    let home_height = height.saturating_sub(PROJECT_BAR_ROWS);
    let home = render_home(home_height, width, &projection);
    let mut preview_deck = deck.clone();
    preview_deck.preview_path(path);
    Some(compose_workspace_shell_frame(
        &preview_deck,
        home_height,
        width,
        &home,
    ))
}

/// Open and refresh a workspace while an interactive production adapter keeps
/// presenting progress.
///
/// # Errors
///
/// Returns loader, terminal, worker, or cancellation failures.
pub fn open_workspace_responsive(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    path: &Path,
    label: &str,
) -> io::Result<WorkspaceSnapshot> {
    if !loader.background_operations() {
        let snapshot = loader.open(path)?;
        return Ok(refresh_empty_workspace_snapshot(loader, snapshot));
    }
    run_workspace_loading(term, label, true, || {
        let snapshot = loader.open(path)?;
        Ok(refresh_empty_workspace_snapshot(loader, snapshot))
    })
}

fn activate_workspace_responsive(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    path: &Path,
    label: &str,
) -> io::Result<()> {
    if loader.background_operations() {
        run_workspace_loading(term, label, false, || loader.activate_prepared(path))
    } else {
        loader.activate_prepared(path)
    }
}

fn activate_cached_workspace_responsive(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    deck: &WorkspaceDeck,
    path: &Path,
    label: &str,
) -> io::Result<()> {
    if !loader.background_operations() || !deck.contains_path(path) {
        return activate_workspace_responsive(term, loader, path, label);
    }
    run_workspace_loading_with(
        term,
        label,
        false,
        true,
        || loader.activate_prepared(path),
        |height, width, frame, status, show_progress| {
            cached_workspace_switch_frame(deck, path, height, width, frame, status, show_progress)
                .expect("the cached workspace path was checked before activation")
        },
    )
}

fn prepare_deck_workspace(
    term: &mut dyn Terminal,
    loader: &mut Option<&mut dyn WorkspaceLoader>,
    deck: &mut WorkspaceDeck,
    path: &Path,
    label: &str,
) -> Option<WorkspaceSnapshot> {
    let Some(loader) = loader.as_mut() else {
        deck.set_notice("Open the workspace list to add or switch projects.");
        return None;
    };
    let result = if (**loader).background_operations() && deck.contains_path(path) {
        run_workspace_loading_with(
            term,
            label,
            true,
            true,
            || {
                let snapshot = (**loader).open(path)?;
                Ok(refresh_empty_workspace_snapshot(&mut **loader, snapshot))
            },
            |height, width, frame, status, show_progress| {
                cached_workspace_switch_frame(
                    deck,
                    path,
                    height,
                    width,
                    frame,
                    status,
                    show_progress,
                )
                .expect("the cached workspace path was checked before opening")
            },
        )
    } else {
        open_workspace_responsive(term, &mut **loader, path, label)
    };
    match result {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
            // A cancelled worker may already have completed its declaration.
            // Reassert the still-visible project before its resident pumps run
            // again, so cancellation never silently switches tenant authority.
            let current = deck.active_path().to_path_buf();
            let _ = activate_cached_workspace_responsive(
                term,
                &mut **loader,
                deck,
                &current,
                "Restoring current workspace…",
            );
            deck.set_notice(error.to_string());
            None
        }
    }
}

fn refresh_empty_workspace_snapshot(
    loader: &mut dyn WorkspaceLoader,
    snapshot: WorkspaceSnapshot,
) -> WorkspaceSnapshot {
    if snapshot.session_ids.is_empty() {
        let path = snapshot.workspace.path.clone();
        return loader.refresh(&path).unwrap_or(snapshot);
    }
    snapshot
}

fn restore_prepared_workspace(loader: &mut Option<&mut dyn WorkspaceLoader>, current: &Path) {
    let Some(loader) = loader.as_mut() else {
        return;
    };
    let _ = (**loader).activate_prepared(current);
}

fn prepare_activation_settings(
    workspace_config: &mut Option<WorkspaceConfigContext<'_>>,
    loader: &mut Option<&mut dyn WorkspaceLoader>,
    deck: &mut WorkspaceDeck,
    current: &Path,
    target: &Path,
) -> bool {
    let Some(context) = workspace_config.as_mut() else {
        return true;
    };
    if let Err(error) = context.settings.select_workspace(target) {
        let _ = context.settings.select_workspace(current);
        restore_prepared_workspace(loader, current);
        deck.set_notice(error.to_string());
        return false;
    }
    true
}

fn prepare_batch_settings(
    workspace_config: &mut Option<WorkspaceConfigContext<'_>>,
    loader: &mut Option<&mut dyn WorkspaceLoader>,
    deck: &mut WorkspaceDeck,
    current: &Path,
    prepared: &[WorkspaceSnapshot],
) -> bool {
    let Some(context) = workspace_config.as_mut() else {
        return true;
    };
    for snapshot in prepared {
        if let Err(error) = context.settings.select_workspace(&snapshot.workspace.path) {
            let _ = context.settings.select_workspace(current);
            restore_prepared_workspace(loader, current);
            deck.set_notice(error.to_string());
            return false;
        }
    }
    true
}

/// Row zero belongs to the project bar. Home continues to receive coordinates
/// relative to its own first row.
fn adjust_project_bar_pointer(key: Key) -> Key {
    match key {
        Key::Click { column, row } if row > 0 => Key::Click {
            column,
            row: row - 1,
        },
        Key::Pointer(mut pointer) if pointer.row > 0 => {
            pointer.row -= 1;
            Key::Pointer(pointer)
        }
        other => other,
    }
}

/// Tracks how long the user has been away from the keyboard.
///
/// The clock itself stays in the frame loop, exactly as it already does for
/// sidebar double-click detection: the shell reduces a monotonic [`Instant`] to
/// an elapsed [`Duration`] and injects it, so neither this unit nor the reducer
/// ever reads the wall clock.
///
/// [`Instant`]: std::time::Instant
#[derive(Debug)]
struct IdleWatch {
    /// Elapsed reading of the shell's monotonic clock at the last interaction.
    since: std::time::Duration,
}

impl IdleWatch {
    const fn new(now: std::time::Duration) -> Self {
        Self { since: now }
    }

    /// Observe one frame's key and return the idle duration to inject.
    fn observe(&mut self, key: &Key, now: std::time::Duration) -> std::time::Duration {
        if is_user_activity(key) {
            self.since = now;
        }
        now.saturating_sub(self.since)
    }
}

fn workspace_navigation_target(
    deck: &WorkspaceDeck,
    state: &AppState,
    key: &Key,
) -> Option<PathBuf> {
    if deck.overlay_open()
        || state.overlay().is_some()
        || state.director_drawer_open()
        || state.root_terminal_drawer_open()
    {
        return None;
    }
    match key {
        Key::Left if state.route() == Route::Home(HomeMode::Switch) => {
            Some(deck.previous_path().to_path_buf())
        }
        Key::Right if state.route() == Route::Home(HomeMode::Switch) => {
            Some(deck.next_path().to_path_buf())
        }
        Key::Live(LiveTerminalAction::PreviousWorkspace) => {
            Some(deck.previous_path().to_path_buf())
        }
        Key::Live(LiveTerminalAction::NextWorkspace) => Some(deck.next_path().to_path_buf()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceDeckHelp {
    None,
    AddWorkspace,
    WorkspaceFinder,
}

impl WorkspaceDeckHelp {
    const fn new(add_open: bool, any_overlay_open: bool) -> Self {
        if add_open {
            Self::AddWorkspace
        } else if any_overlay_open {
            Self::WorkspaceFinder
        } else {
            Self::None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceBaseHelp {
    Switch,
    Closeup,
    LiveTerminal,
}

impl WorkspaceBaseHelp {
    const fn new(route: Route, live_input: bool) -> Self {
        match route {
            Route::Home(HomeMode::Switch) => Self::Switch,
            Route::Home(HomeMode::Closeup) if live_input => Self::LiveTerminal,
            Route::Home(HomeMode::Closeup) => Self::Closeup,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceHelpState {
    deck: WorkspaceDeckHelp,
    overlay: Option<Overlay>,
    decision_answer_open: bool,
    work_run_mode: WorkRunControlMode,
    director_new_open: bool,
    director_route: DirectorRoute,
    drawer_focus: Option<WorkspaceDrawerFocus>,
    base: WorkspaceBaseHelp,
}

fn resolve_workspace_help_context(state: WorkspaceHelpState) -> KeyHelpContext {
    match state.deck {
        WorkspaceDeckHelp::AddWorkspace => return KeyHelpContext::AddWorkspace,
        WorkspaceDeckHelp::WorkspaceFinder => return KeyHelpContext::WorkspaceFinder,
        WorkspaceDeckHelp::None => {}
    }
    if let Some(overlay) = state.overlay {
        return match overlay {
            Overlay::Overview => KeyHelpContext::Overview,
            Overlay::Daemon => KeyHelpContext::Daemon,
            Overlay::Closeup => KeyHelpContext::CloseupActions,
            Overlay::QuitConfirmation => KeyHelpContext::ExitConfirmation,
            Overlay::ForceRemoveConfirmation => KeyHelpContext::ForceRemove,
            Overlay::Notes => KeyHelpContext::Scratchpad,
            Overlay::Environment => KeyHelpContext::WorkspaceEnvironmentEditor,
            Overlay::Roles => KeyHelpContext::RolesEditor,
            Overlay::CreateSession => KeyHelpContext::CreateSession,
            Overlay::Decisions if state.decision_answer_open => KeyHelpContext::DecisionAnswer,
            Overlay::Decisions => KeyHelpContext::DecisionList,
            Overlay::CleanupQueue => KeyHelpContext::CleanupQueue,
            Overlay::RemoveSessions => KeyHelpContext::RemoveSessions,
            Overlay::Prs => KeyHelpContext::PullRequests,
            Overlay::Preview => KeyHelpContext::Preview,
            Overlay::CreateSessionError => KeyHelpContext::CreateSessionError,
            Overlay::TerminalLaunchError => KeyHelpContext::TerminalLaunchError,
            Overlay::AgentLaunchError => KeyHelpContext::AgentLaunchError,
            Overlay::Garden => KeyHelpContext::Garden,
        };
    }
    if state.drawer_focus == Some(WorkspaceDrawerFocus::Director)
        && state.work_run_mode == WorkRunControlMode::Submitting
    {
        return KeyHelpContext::WorkRunSubmitting;
    }
    if state.drawer_focus == Some(WorkspaceDrawerFocus::Director)
        && matches!(
            state.director_route,
            DirectorRoute::WorkRuns | DirectorRoute::RunOverview(_)
        )
    {
        match state.work_run_mode {
            WorkRunControlMode::List if state.director_route == DirectorRoute::WorkRuns => {
                return KeyHelpContext::WorkRuns;
            }
            WorkRunControlMode::List => return KeyHelpContext::RunOverview,
            WorkRunControlMode::ResolveEscalation => return KeyHelpContext::WorkRunEscalation,
            WorkRunControlMode::ConfirmCancel
            | WorkRunControlMode::ConfirmDelete
            | WorkRunControlMode::Retry => return KeyHelpContext::WorkRunConfirmation,
            WorkRunControlMode::Submitting | WorkRunControlMode::Closed => {}
        }
    }
    if state.director_new_open && state.drawer_focus == Some(WorkspaceDrawerFocus::Director) {
        return KeyHelpContext::DirectorNew;
    }
    match state.drawer_focus {
        Some(WorkspaceDrawerFocus::Director) => {
            return match state.director_route {
                DirectorRoute::Organization => KeyHelpContext::Organization,
                DirectorRoute::WorkRuns => KeyHelpContext::WorkRuns,
                DirectorRoute::RunOverview(_) => KeyHelpContext::RunOverview,
                DirectorRoute::Console(DirectorConsoleParent::Organization) => {
                    KeyHelpContext::DirectorConsole
                }
                DirectorRoute::Console(DirectorConsoleParent::RunOverview(_)) => {
                    KeyHelpContext::WorkRunConsole
                }
            };
        }
        Some(WorkspaceDrawerFocus::Terminal) => return KeyHelpContext::RootShell,
        None => {}
    }
    match state.base {
        WorkspaceBaseHelp::Switch => KeyHelpContext::Switch,
        WorkspaceBaseHelp::Closeup => KeyHelpContext::Closeup,
        WorkspaceBaseHelp::LiveTerminal => KeyHelpContext::LiveTerminal,
    }
}

/// Resolve the frontmost visible surface before Help opens. The resulting
/// value is held for the lifetime of that Help overlay, so background daemon
/// updates cannot make the shortcut list jump while it is being read.
fn workspace_help_context(
    deck: &WorkspaceDeck,
    runtime: &WorkspaceRuntime,
    work_run_control: &WorkRunControl,
) -> KeyHelpContext {
    let state = runtime.state();
    resolve_workspace_help_context(WorkspaceHelpState {
        deck: WorkspaceDeckHelp::new(deck.add_overlay_open(), deck.overlay_open()),
        overlay: state.overlay(),
        decision_answer_open: state
            .decision_overlay()
            .and_then(DecisionOverlayState::editor)
            .is_some(),
        work_run_mode: work_run_control.mode(),
        director_new_open: state.director_launching().is_some()
            || state.director_new() != DirectorNew::Idle,
        director_route: state.director_route(),
        drawer_focus: state.workspace_drawer_focus(),
        base: WorkspaceBaseHelp::new(state.route(), runtime.wants_live_input()),
    })
}

/// Whether one workspace key opens the contextual keyboard-help overlay.
///
/// `Ctrl-?` remains global. Plain `?` is the discoverable shortcut on an
/// unobscured management Home, while a focused live terminal preserves plain
/// `?` for the PTY and uses the `Ctrl-O ?` action instead.
fn opens_workspace_help(
    key: &Key,
    deck: &WorkspaceDeck,
    runtime: &WorkspaceRuntime,
    work_run_control: &WorkRunControl,
) -> bool {
    match key {
        Key::Help | Key::Live(LiveTerminalAction::KeyboardHelp) => true,
        Key::Char('?') => {
            !deck.overlay_open()
                && runtime.state().overlay().is_none()
                && runtime.state().workspace_drawer_focus().is_none()
                && work_run_control.mode() == WorkRunControlMode::Closed
                && !runtime.wants_live_input()
        }
        _ => false,
    }
}

fn closes_workspace_help(key: &Key, context: KeyHelpContext) -> bool {
    matches!(
        key,
        Key::Help | Key::Escape | Key::Live(LiveTerminalAction::KeyboardHelp)
    ) || matches!(key, Key::Char('?'))
        && (context == KeyHelpContext::Switch || context == KeyHelpContext::Closeup)
}

fn scroll_key_help(state: &mut key_help::State, key: &Key, height: usize) -> bool {
    let page = height.saturating_sub(8).max(1);
    match key {
        Key::Up => state.scroll_up(1),
        Key::Down => state.scroll_down(1),
        Key::PageUp => state.scroll_up(page),
        Key::PageDown => state.scroll_down(page),
        Key::Home | Key::LineStart => state.scroll_home(),
        Key::End | Key::LineEnd => state.scroll_end(),
        _ => return false,
    }
    true
}

/// Maximum number of completions one Home frame may apply from each queue.
const FRAME_EVENT_BUDGET: usize = 128;

/// Registry reads are useful only while `+ Open` is visible. A short bounded
/// cadence feels live across processes without turning the Home frame loop into
/// a filesystem poller.
const REGISTRY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Controller-driven real-terminal frame loop (`drain → poll → render → input →
/// dispatch`). Home row state, live-pane availability, and the Home frame come
/// from [`WorkspaceRuntime`]/`render_home`; [`WorkspaceIoRuntime`] holds only
/// daemon transport coordination (session workers, pane launches, terminal
/// streams, metrics) and owns no route or selection state.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=screen_graph_production_port_harness
fn drive_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    deck: &mut WorkspaceDeck,
    registry: &[Workspace],
    mut loader: Option<&mut dyn WorkspaceLoader>,
    backend_factory: &mut dyn ControllerBackendFactory,
    modal_selection_mode: usagi_core::domain::settings::ModalSelectionMode,
    pr_auto_open: usagi_core::domain::settings::PrAutoOpen,
    entry_policy: WorkspaceEntryPolicy,
    mut workspace_config: Option<WorkspaceConfigContext<'_>>,
) -> io::Result<WorkspaceStep> {
    let mut registry = registry.to_vec();
    let WorkspaceEntryPolicy {
        available_models,
        default_model,
        default_branch,
        work_mode,
        icon_mode,
    } = entry_policy;
    deck.set_icon_mode(icon_mode);
    let workspace_id = snapshot.workspace_id;
    let session_ids = snapshot.session_ids.clone();
    let workspace_name = snapshot.workspace.name.clone();
    let root_cwd = snapshot.workspace.path.clone();
    let agent_resumes = snapshot.agent_resumes.clone();
    let session_lifecycles = snapshot.session_lifecycles.clone();
    let garden_reduced_motion = backend_factory.garden_reduced_motion();
    let (host, host_rx) = ControllerHost::channel();
    let composition = backend_factory.create(&snapshot, host);
    let mut backend = composition.backend;
    let mut browser = composition.browser;
    let mut restore_commands = Some(composition.restore_commands);
    let mut restore_connection = composition.restore_connection;
    // Cross-project Garden observation. Its dedicated port is parked here while
    // no round is in flight, exactly like the restore lane's.
    let mut garden_inventory = Some(composition.garden_inventory);
    let mut work_run_port = Some(composition.work_runs);
    // Resident session-inventory lane. The frame loop only wakes and drains it;
    // the observation itself never runs here (#551).
    let mut session_refresh = composition.session_refresh;
    // The `Effect::RefreshSessions` completion parked until the resident lane
    // publishes its next snapshot. Requests inside one cadence period coalesce
    // onto that one snapshot instead of each issuing a request, and every
    // completion sink of a workspace is a clone of the same channel, so keeping
    // the newest is keeping all of them.
    let mut pending_session_refresh: Option<Completions> = None;
    let (restore_sender, restore_completions) = mpsc::channel();
    let (garden_sender, garden_completions) = mpsc::channel::<GardenObservationCompletion>();
    let (work_run_sender, work_run_completions) = mpsc::channel::<WorkRunLaneCompletion>();
    let mut workspace =
        WorkspaceView::with_runtime_ids(snapshot.workspace, snapshot.state, session_ids.clone());
    workspace.set_session_lifecycles(session_lifecycles);
    let mut ui = WorkspaceIoRuntime::new(workspace, composition.session_commands)
        .with_agent_resumes(agent_resumes)
        .with_agent_context(
            workspace_id,
            session_ids.clone(),
            composition.agent_commands,
        )
        .with_pane_launch_port(composition.pane_launch_commands)
        .with_agent_tab_intent(
            workspace_id,
            session_ids.iter().copied().collect(),
            composition.agent_tab_intents,
        )
        .with_external_terminal(composition.external_terminal);
    let mut runtime =
        WorkspaceRuntime::with_selection_mode(workspace_id, session_ids, modal_selection_mode);
    restore_workspace_session_focus(deck, &root_cwd, &mut runtime);
    let mut pending_garden_visit = deck.take_garden_visit(&root_cwd);
    let mut pending_garden_agent = None;
    runtime.set_pr_auto_open(pr_auto_open);
    let data_home = usagi_core::infrastructure::paths::data_dir().ok();
    let role_catalog = session_role_catalog(data_home.as_deref(), &root_cwd);
    let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionRoleCatalog(
        role_catalog,
    )));
    // Git ref discovery can be slow on large or remote filesystems. Start Home
    // immediately with the daemon's HEAD default, then reflux the catalog from a
    // one-shot worker without ever holding the render thread.
    let (branch_catalog_sender, branch_catalog_receiver) = mpsc::channel();
    let branch_catalog_root = root_cwd.clone();
    let branch_catalog_default = default_branch.clone();
    let _ = std::thread::Builder::new()
        .name("tui-branch-catalog".to_owned())
        .spawn(move || {
            let _ = branch_catalog_sender.send(session_branch_catalog(
                &branch_catalog_root,
                branch_catalog_default.as_deref(),
            ));
        });
    runtime.set_agent_models(available_models, default_model);
    runtime.set_work_mode(work_mode);
    if let Some(error) = ui.take_agent_tab_intent_load_error() {
        surface_agent_tab_intent_error(&mut runtime, error);
    }
    let mut metrics_backend = MetricsBackend::new(composition.metrics);
    let mut metrics_projection = MetricsProjection::default();
    let mut pending_targets: std::collections::HashMap<OperationId, Target> =
        std::collections::HashMap::new();
    // The reducer hit-tests sidebar clicks and owns stable-identity double-click
    // state. The shell's clock is reduced to a deterministic elapsed timestamp.
    let pointer_clock = std::time::Instant::now();
    // The screen saver's idle deadline rides the same clock: the shell observes
    // user input and monotonic time, the reducer only sees an injected duration.
    let mut idle_watch = IdleWatch::new(pointer_clock.elapsed());
    // A Garden mouse-down owns the complete drag/release gesture even though
    // the down itself closes the overlay. Later gesture phases must not land on
    // the newly revealed terminal or sidebar.
    let mut garden_pointer_gesture = false;
    // Live-terminal scroll offset, drag selection, and copy feedback the reducer
    // does not own (design §4.2).
    let mut controls = LiveTerminalControls::default();
    // Seed the daemon-authoritative snapshots before the first frame so a
    // pending decision and another client's sessions are visible without
    // requiring a manual key binding. Both are wakes of a resident lane, not
    // synchronous requests: the frame loop issues no daemon RPC of its own
    // (#551).
    let _ = backend.dispatch(Effect::RefreshDecisions {
        workspace: workspace_id,
    });
    let _ = backend.dispatch(Effect::SyncPullRequestTargets {
        sessions: runtime.state().sessions().to_vec(),
    });
    session_refresh.wake();
    // Start restore after the first frame. The controller owns retry admission
    // and a capped backoff across worker jobs; a frame tick never resets it.
    let restore_clock = std::time::Instant::now();
    let mut restore_retry = RestoreRetryState::new();
    let mut registry_refresh_pending = false;
    let mut registry_refresh_due = std::time::Duration::ZERO;
    let mut garden_observation =
        ObservationLane::new(GARDEN_OBSERVATION_INTERVAL, GARDEN_OBSERVATION_BACKOFF);
    let mut garden_observations = 0_u64;
    let mut work_run_observation =
        ObservationLane::new(WORK_RUN_OBSERVATION_INTERVAL, WORK_RUN_OBSERVATION_BACKOFF);
    let mut work_runs = WorkRunProjection::default();
    let mut work_run_control = WorkRunControl::default();
    let mut help_context: Option<key_help::State> = None;
    let mut pending_work_run_control = None;
    let mut work_run_revision = 0_u64;
    // Filesystem hint for the inline create form. It is off the frame budget:
    // no scan happens while the form is closed (#554).
    let mut worktree_hint = SessionWorktreeHint::new(composition.session_worktrees);
    // Material of the frame currently on screen. A tick whose material matches
    // it draws nothing: the frame build and the terminal diff are both skipped.
    // Everything else in this loop — drains, admission, input — runs regardless.
    let mut drawn_material: Option<HomeFrameMaterial> = None;
    // Owned daemon row/path material is rebuilt only when its authoritative
    // inputs change. The cache never feeds commands back into the controller.
    let mut session_material_key: Option<(u64, Option<SessionId>, u64)> = None;
    let mut sessions: Arc<[ProjectedSession]> = Arc::from([]);
    let mut metrics_sessions = Vec::new();
    let mut terminal_material_key: Option<(Option<TerminalRef>, u64, u64, Geometry)> = None;
    let mut terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut terminal_rows_len = 0;
    let mut terminal_scroll = 0;
    let mut terminal_generation = 0_u64;
    let mut background_terminal_material_key = None;
    let mut background_terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut background_terminal_generation = 0_u64;
    let mut director_terminal_material_key = None;
    let mut director_terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut director_terminal_generation = 0_u64;
    let mut root_terminal_material_key = None;
    let mut root_terminal_view: Option<Arc<TerminalViewProjection>> = None;
    let mut root_terminal_generation = 0_u64;
    let mut director_material_key = None;
    // The source key remembers the raw Garden cadence so a held pose is
    // canonicalized once. The material key stores only the canonical visible
    // tick and therefore names the frame actually sent to the terminal.
    let mut frame_source_key: Option<FrameMaterialKey> = None;
    let mut frame_material_key: Option<FrameMaterialKey> = None;
    let mut allowed_sessions_revision = u64::MAX;
    let mut current_sessions = BTreeSet::new();
    loop {
        let registry_now = restore_clock.elapsed();
        if deck.add_overlay_open()
            && !registry_refresh_pending
            && registry_now >= registry_refresh_due
            && let Some(loader) = loader.as_mut()
        {
            match (**loader).dispatch_registry_refresh() {
                Ok(true) => registry_refresh_pending = true,
                Ok(false) => {
                    registry_refresh_due = registry_now + REGISTRY_REFRESH_INTERVAL;
                }
                Err(error) => {
                    deck.set_notice(error.to_string());
                    registry_refresh_due = registry_now + REGISTRY_REFRESH_INTERVAL;
                    drawn_material = None;
                    frame_source_key = None;
                    frame_material_key = None;
                }
            }
        }
        if let Some(completion) = loader
            .as_mut()
            .and_then(|loader| (**loader).take_registry_refresh())
        {
            registry_refresh_pending = false;
            registry_refresh_due = registry_now + REGISTRY_REFRESH_INTERVAL;
            match completion {
                Ok(latest) => {
                    registry = latest;
                    if deck.add_overlay_open() {
                        deck.refresh_add(&registry);
                        drawn_material = None;
                        frame_source_key = None;
                        frame_material_key = None;
                    }
                }
                Err(error) if deck.add_overlay_open() => {
                    deck.set_notice(error.to_string());
                    drawn_material = None;
                    frame_source_key = None;
                    frame_material_key = None;
                }
                Err(_) => {}
            }
        }
        if let Ok(catalog) = branch_catalog_receiver.try_recv() {
            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionBranchCatalog(
                catalog,
            )));
        }
        for event in backend.drain_events_bounded(FRAME_EVENT_BUDGET) {
            let _ = runtime.apply_event(event);
        }
        while let Some(epoch) = restore_connection.take_reconnected_epoch() {
            restore_retry.reconnected(epoch, restore_clock.elapsed());
        }
        drain_controller_host_actions(
            &host_rx,
            &mut ui,
            &mut runtime,
            &mut pending_targets,
            session_refresh.as_mut(),
            &mut pending_session_refresh,
        );
        if ui.take_agent_observation_request() {
            restore_retry.request_observation(restore_clock.elapsed());
        }
        if ui.take_agent_inventory_change_observation_request() {
            restore_retry.request_changed_observation(restore_clock.elapsed());
        }
        drain_session_completions(&mut ui);
        drain_session_refresh(
            &mut ui,
            session_refresh.as_mut(),
            &mut pending_session_refresh,
        );
        let worktree_names = worktree_hint.names(
            runtime.state().create_session_form().is_some(),
            ui.workspace.path(),
            restore_clock.elapsed(),
        );
        sync_runtime_sessions(&mut runtime, &ui, worktree_names);
        restore_workspace_closeup(deck, &root_cwd, &mut runtime);
        if let Some(visit) = pending_garden_visit.take() {
            let _ = runtime.apply_event(AppEvent::VisitSession(visit.session));
            pending_garden_agent = visit.agent.map(|agent| (visit.session, agent));
        }
        let workspace_material_revision = ui.workspace.material_revision();
        if allowed_sessions_revision != workspace_material_revision {
            current_sessions = ui.workspace.session_ids().iter().copied().collect();
            ui.set_allowed_agent_sessions(current_sessions.iter().copied());
            allowed_sessions_revision = workspace_material_revision;
        }
        for completion in restore_completions.try_iter().take(FRAME_EVENT_BUDGET) {
            let applied = apply_restore_completion(
                completion,
                &mut ui,
                &mut runtime,
                workspace_id,
                &current_sessions,
            );
            let outcome = applied.outcome;
            let show_notice = restore_retry.complete(restore_clock.elapsed(), outcome);
            restore_commands = Some(applied.port);
            if show_notice {
                let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Notice(Notice::new(
                    "daemon restore is unavailable after retries; no Agent was started",
                ))));
            }
            if let RestoreJobOutcome::IntentFailed(error) = outcome {
                surface_agent_tab_intent_error(&mut runtime, error);
            }
        }
        if let Some((session, agent)) = pending_garden_agent {
            let selected = visit_garden_agent(
                &mut ui,
                &mut runtime,
                GardenClick::Visit {
                    workspace: workspace_id,
                    session,
                    agent: Some(agent),
                },
            );
            if selected {
                activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
            }
            if selected || ui.agent_inventory().is_some() {
                pending_garden_agent = None;
            }
        }
        for completion in garden_completions.try_iter().take(FRAME_EVENT_BUDGET) {
            let observed = !completion.inventories.is_empty();
            let mut changed = false;
            for inventory in &completion.inventories {
                changed |= deck.apply_garden_inventory(inventory);
            }
            if changed {
                garden_observations = garden_observations.wrapping_add(1);
            }
            garden_inventory = Some(completion.port);
            garden_observation.complete(restore_clock.elapsed(), observed);
        }
        for completion in work_run_completions.try_iter().take(FRAME_EVENT_BUDGET) {
            match completion {
                WorkRunLaneCompletion::Observation { port, snapshot } => {
                    let observed = snapshot.is_ok();
                    let next = match snapshot {
                        Ok(snapshot) => WorkRunProjection::fresh(snapshot.runs),
                        Err(_) => work_runs.clone().unavailable(),
                    };
                    if next != work_runs {
                        work_runs = next;
                        work_run_control.sync_selection(work_runs.runs());
                        work_run_revision = work_run_revision.wrapping_add(1);
                    }
                    work_run_port = Some(port);
                    work_run_observation.complete(restore_clock.elapsed(), observed);
                }
                WorkRunLaneCompletion::Control {
                    port,
                    operation_id,
                    result,
                } => {
                    let result = *result;
                    let previous_control = work_run_control.clone();
                    let accepted = work_run_control.complete(operation_id, &result);
                    if accepted && let Ok(result) = &result {
                        match result {
                            WorkRunControlResult::Updated(run) => {
                                work_runs.apply_control(run.as_ref().clone());
                            }
                            WorkRunControlResult::Deleted(deletion) => {
                                work_runs.apply_deletion(*deletion);
                                work_run_control.sync_selection(work_runs.runs());
                            }
                        }
                    }
                    if work_run_control != previous_control || accepted {
                        work_run_revision = work_run_revision.wrapping_add(1);
                    }
                    work_run_port = Some(port);
                    work_run_observation.refresh_now();
                }
            }
        }
        let (terminal_height, width) = term.size()?;
        let height = terminal_height.saturating_sub(PROJECT_BAR_ROWS);
        ui.set_terminal_size(height, width);
        let _ = runtime.apply_event(AppEvent::Resize {
            width: u16::try_from(width).unwrap_or(u16::MAX),
            height: u16::try_from(height).unwrap_or(u16::MAX),
        });
        let garden_available = garden_fits(height, width);
        if runtime.state().overlay() == Some(Overlay::Garden) && !garden_available {
            let _ = runtime.apply_event(AppEvent::GardenUnavailable);
        }
        let _ = runtime.apply_event(AppEvent::GardenAvailability(garden_available));
        let geometry = foreground_terminal_geometry(
            height,
            width,
            runtime.state().director_drawer_open(),
            runtime.state().root_terminal_drawer_open(),
            runtime.state().root_terminal_full_height(),
            runtime.state().workspace_drawer_focus(),
        );
        drain_pane_completions_into_runtime(&mut ui, &mut runtime, &mut pending_targets, geometry);
        // Keep every terminal in the Home composition attached. Concurrent
        // Director and root-terminal drawers add two root surfaces above the
        // managed-session Agent instead of replacing, resizing, or detaching it.
        let director_terminal = runtime.director_terminal();
        let root_terminal = runtime.root_terminal();
        let terminal_attachments = workspace_terminal_attachments(&runtime, height, width);
        ui.sync_visible_terminals(&terminal_attachments);
        // Polling still runs every tick so output/admission progresses, but row
        // String creation and URL scanning run only behind the projection key.
        close_exited_panes(&mut ui, &mut runtime);
        sync_terminal_selection_motions(&mut ui, &mut controls);
        let focused_terminal = runtime.preview_terminal();
        let mut live_terminals = runtime.background_terminals();
        if let Some(terminal) = &focused_terminal {
            live_terminals.push(terminal.clone());
        }
        controls.retain_terminals(&live_terminals);
        controls.sync_focus(focused_terminal.as_ref());
        let screen_revision = focused_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let next_terminal_key = (
            focused_terminal.clone(),
            screen_revision,
            controls.revision(),
            geometry,
        );
        if terminal_material_key.as_ref() != Some(&next_terminal_key) {
            terminal_view =
                controller_terminal_view(&ui, &runtime, &mut controls, usize::from(geometry.rows))
                    .map(Arc::new);
            (terminal_rows_len, terminal_scroll) = match &terminal_view {
                Some(view) => (view.total_rows, controls.scroll()),
                None => (0, 0),
            };
            terminal_material_key = Some(next_terminal_key);
            terminal_generation = terminal_generation.saturating_add(1);
        }
        let background_terminal = managed_background_terminal(&runtime);
        let background_revision = background_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let background_rows = usize::from(managed_background_terminal_geometry(height, width).rows);
        let next_background_key = (
            background_terminal.clone(),
            background_revision,
            background_rows,
        );
        if background_terminal_material_key.as_ref() != Some(&next_background_key) {
            background_terminal_view = background_terminal
                .as_ref()
                .and_then(|terminal| ui.retained_terminal_view(terminal, background_rows))
                .map(Arc::new);
            background_terminal_material_key = Some(next_background_key);
            background_terminal_generation = background_terminal_generation.saturating_add(1);
        }
        let director_rows = director_drawer::terminal_viewport(height, width).rows;
        let director_revision = director_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let director_focused = director_terminal.as_ref().is_some_and(|director| {
            focused_terminal
                .as_ref()
                .is_some_and(|focused| focused.fences(director))
        });
        let next_director_terminal_key = (
            director_terminal.clone(),
            director_revision,
            director_rows,
            director_focused,
            terminal_generation,
        );
        if director_terminal_material_key.as_ref() != Some(&next_director_terminal_key) {
            director_terminal_view = if director_focused {
                terminal_view.as_ref().map(Arc::clone)
            } else {
                director_terminal
                    .as_ref()
                    .and_then(|terminal| ui.retained_terminal_view(terminal, director_rows))
                    .map(Arc::new)
            };
            director_terminal_material_key = Some(next_director_terminal_key);
            director_terminal_generation = director_terminal_generation.saturating_add(1);
        }
        let root_rows = root_terminal_drawer::terminal_viewport_for(
            height,
            width,
            workspace::root_terminal_available_width(
                height,
                width,
                runtime.state().director_drawer_open(),
            ),
        )
        .rows;
        let root_revision = root_terminal
            .as_ref()
            .and_then(|terminal| ui.terminal_projection_key(terminal))
            .unwrap_or(0);
        let root_focused = root_terminal.as_ref().is_some_and(|root| {
            focused_terminal
                .as_ref()
                .is_some_and(|focused| focused.fences(root))
        });
        let next_root_terminal_key = (
            root_terminal.clone(),
            root_revision,
            root_rows,
            root_focused,
            terminal_generation,
        );
        if root_terminal_material_key.as_ref() != Some(&next_root_terminal_key) {
            root_terminal_view = if root_focused {
                terminal_view.as_ref().map(Arc::clone)
            } else {
                root_terminal
                    .as_ref()
                    .and_then(|terminal| ui.retained_terminal_view(terminal, root_rows))
                    .map(Arc::new)
            };
            root_terminal_material_key = Some(next_root_terminal_key);
            root_terminal_generation = root_terminal_generation.saturating_add(1);
        }
        let next_director_key = (
            runtime.material_key(),
            ui.material_revision,
            director_terminal_generation,
            work_run_revision,
        );
        if director_material_key != Some(next_director_key) {
            let drawer_projection =
                director_drawer_projection(&ui, &runtime, director_terminal_view.as_deref())
                    .with_work_runs(work_runs.clone())
                    .with_work_run_control(work_run_control_projection(&work_run_control));
            runtime.set_director_projection(drawer_projection);
            director_material_key = Some(next_director_key);
        }
        if ui.take_terminal_reconnected() {
            let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::Feedback(
                Feedback::Reconnected,
            )));
        }
        let next_session_key = (
            ui.workspace.material_revision(),
            ui.removing_session,
            runtime.state().session_pr_revision(),
        );
        let sessions_changed = session_material_key != Some(next_session_key);
        if sessions_changed {
            sessions = Arc::from(project_controller_sessions(&ui, runtime.state()));
            deck.update_active_sessions(&sessions);
            metrics_sessions = sessions
                .iter()
                .map(|session| (session.id, session.cwd.clone()))
                .collect();
            session_material_key = Some(next_session_key);
        }
        // Reflux daemon metrics / git diffs through the backend drain instead of
        // polling the port inline: the shell folds the updates into its own
        // projection cache, so the material does not ride on the IO runtime.
        metrics_backend.poll(sessions_changed.then_some(metrics_sessions.as_slice()));
        for update in metrics_backend.drain_events() {
            metrics_projection.apply(update);
        }
        let now = relative_time_clock(Utc::now());
        let garden_open = runtime.state().overlay() == Some(Overlay::Garden);
        let drives_tick_animation = ui.creating_session.is_some()
            || sessions.iter().any(|session| session.removing)
            || runtime
                .preview_pane()
                .tabs()
                .iter()
                .any(|tab| matches!(tab, PaneTab::Pending(_)));
        let animation = if garden_reduced_motion {
            0
        } else if garden_open {
            widgets::garden::runtime_tick(runtime.state().mascot_tick())
        } else if drives_tick_animation {
            runtime.state().mascot_tick()
        } else {
            widgets::mascot::canonical_tick(runtime.state().mascot_tick())
        };
        let next_source_key = FrameMaterialKey {
            height,
            width,
            controller: runtime.material_key(),
            sessions: next_session_key,
            shell: ui.material_revision,
            metrics: metrics_projection.generation(),
            terminal: (
                terminal_generation,
                background_terminal_generation,
                director_terminal_generation,
                root_terminal_generation,
            ),
            animation,
            create_pending: ui
                .creating_session
                .as_ref()
                .map(|create| create.name.clone()),
            garden_observations,
            work_run_revision,
            now,
        };
        if frame_source_key.as_ref() != Some(&next_source_key) {
            let material = home_frame_material_shared(
                height,
                width,
                &runtime,
                &workspace_name,
                Arc::clone(&sessions),
                metrics_projection.metrics(),
                metrics_projection.health(),
                metrics_projection.shared_git_diffs(),
                if runtime.state().workspace_drawer_open() {
                    background_terminal_view.as_ref().map(Arc::clone)
                } else {
                    terminal_view.as_ref().map(Arc::clone)
                },
                root_terminal_view.as_deref(),
                ui.creating_session
                    .as_ref()
                    .map(|create| create.name.as_str()),
                now,
                icon_mode,
            )
            .with_agent_inventory(ui.agent_inventory(), runtime.panes())
            .with_work_runs(work_runs.clone())
            .with_workspace_deck_garden(deck)
            .with_garden_animation(animation, garden_reduced_motion);
            let mut next_frame_key = next_source_key.clone();
            if garden_open {
                next_frame_key.animation = material
                    .projection
                    .garden_animation_tick()
                    .unwrap_or(animation);
            }
            let frame_changed = frame_material_key.as_ref() != Some(&next_frame_key);
            let controller_may_be_noop = frame_material_key
                .as_ref()
                .is_some_and(|previous| previous.differs_only_by_controller(&next_frame_key));
            // Skip only the drawing. A skipped tick has already run every drain
            // above and still runs restore admission, pane launches, and input
            // below, so nothing that makes progress depends on the redraw.
            if frame_changed
                && (!controller_may_be_noop || drawn_material.as_ref() != Some(&material))
            {
                let home = render_home_material(&material);
                let frame = compose_workspace_shell_frame(deck, height, width, &home);
                let frame = match help_context {
                    Some(help) => key_help::render_over(terminal_height, width, &frame, help),
                    None => frame,
                };
                term.draw(&frame)?;
                drawn_material = Some(material);
            }
            frame_source_key = Some(next_source_key);
            frame_material_key = Some(next_frame_key);
        }
        // The other open projects' Agents, observed only while the Garden is the
        // frame. The active project keeps its own controller's richer phases.
        if garden_inventory.is_some()
            && garden_observation.begin_if_due(garden_open, restore_clock.elapsed())
        {
            let targets = deck.observable_workspaces();
            if targets.is_empty() {
                // The only open project is the one this loop already draws.
                garden_observation.complete(restore_clock.elapsed(), true);
            } else {
                let port = garden_inventory
                    .take()
                    .expect("the Garden observation port was checked above");
                spawn_garden_observation_job(port, targets, garden_sender.clone());
            }
        }
        if work_run_port.is_some() && pending_work_run_control.is_some() {
            let port = work_run_port
                .take()
                .expect("the Work Run control port was checked above");
            let request = pending_work_run_control
                .take()
                .expect("the Work Run control request was checked above");
            spawn_work_run_control_job(port, workspace_id, request, work_run_sender.clone());
        } else if work_run_port.is_some()
            && work_run_observation.begin_if_due(true, restore_clock.elapsed())
        {
            let port = work_run_port
                .take()
                .expect("the Work Run observation port was checked above");
            spawn_work_run_observation_job(port, workspace_id, work_run_sender.clone());
        }
        if restore_commands.is_some() && restore_retry.begin_if_due(restore_clock.elapsed()) {
            let port = restore_commands
                .take()
                .expect("restore admission checked the dedicated port");
            let (interaction, registry_revision) = runtime.restore_fence();
            spawn_restore_job(
                port,
                workspace_id,
                current_sessions.clone(),
                interaction,
                registry_revision,
                restore_sender.clone(),
            );
        }
        drain_pane_launches(&mut ui, geometry);
        // Contextual New is retargeted once here so PTY forwarding, pane
        // controls, and the reducer all see one normalized key.
        let raw_key = retarget_drawer_chords(&runtime, term.read_key()?);
        if handle_interrupted_removal_confirmation(&raw_key, &mut ui, &mut runtime) {
            // This prompt is the exclusive frontmost owner. In particular,
            // Ctrl-D and pane-close chords never reach a live tab behind it.
            continue;
        }
        if let Some(help) = help_context.as_mut() {
            if closes_workspace_help(&raw_key, help.context()) {
                help_context = None;
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
            } else if scroll_key_help(help, &raw_key, terminal_height) {
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
            }
            // Help is the exclusive frontmost input owner. Background drains
            // keep running on the next loop, but no command reaches the surface
            // being described.
            continue;
        }
        if opens_workspace_help(&raw_key, deck, &runtime, &work_run_control) {
            help_context = Some(key_help::State::new(
                workspace_help_context(deck, &runtime, &work_run_control),
                runtime.state().work_mode(),
            ));
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if dismiss_pr_modal_on_project_bar_click(&mut runtime, &raw_key) {
            continue;
        }
        let bar_click = matches!(raw_key, Key::Click { row: 0, .. });
        let bar_target = match &raw_key {
            Key::Click { column, row: 0 } => project_bar(deck, width)
                .target_at(usize::from(*column))
                .cloned(),
            _ => None,
        };
        let key = adjust_project_bar_pointer(raw_key);

        // Process shortcuts and the remaining project-bar clicks win over the
        // unobscured workspace surface, including a focused live PTY. Foreground
        // overlays/drawers retain ownership through the target resolver below.
        // Plain arrows are deliberately local to unobscured Switch mode.
        let workspace_navigation_target = workspace_navigation_target(deck, runtime.state(), &key);
        let direct_target = match (&key, &bar_target) {
            (Key::Live(LiveTerminalAction::ActivateWorkspace(number)), _) => deck
                .path_at(usize::from(number.saturating_sub(1)))
                .map(Path::to_path_buf),
            (_, Some(ProjectBarTarget::Workspace(path))) => Some(path.clone()),
            _ => workspace_navigation_target,
        };
        let opens_add = matches!(key, Key::Live(LiveTerminalAction::OpenWorkspace))
            || matches!(bar_target, Some(ProjectBarTarget::Add));
        if opens_add {
            deck.open_add(&registry);
            registry_refresh_due = std::time::Duration::ZERO;
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if matches!(key, Key::Live(LiveTerminalAction::OpenWorkspaceSwitcher)) {
            deck.open_switcher();
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if bar_click && direct_target.is_none() {
            continue;
        }
        if let Some(path) = direct_target {
            if path == deck.active_path() {
                continue;
            }
            if workspace_has_unsaved_surface(&runtime) {
                deck.open_switcher();
                deck.set_notice("Save or cancel the current draft before switching.");
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
                continue;
            }
            let preserve_closeup = matches!(
                key,
                Key::Live(
                    LiveTerminalAction::PreviousWorkspace | LiveTerminalAction::NextWorkspace
                )
            ) && runtime.state().route() == Route::Home(HomeMode::Closeup);
            if let Some(prepared) =
                prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                && prepare_activation_settings(
                    &mut workspace_config,
                    &mut loader,
                    deck,
                    &root_cwd,
                    &prepared.workspace.path,
                )
            {
                remember_workspace_session_focus(deck, runtime.state());
                if preserve_closeup {
                    deck.schedule_closeup(prepared.workspace.path.clone());
                }
                return Ok(WorkspaceStep::Activate(Box::new(prepared)));
            }
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if deck.overlay_open() {
            match deck.handle_overlay_key(&key) {
                OverlayIntent::Stay => {}
                OverlayIntent::Cancel => deck.close_overlay(),
                OverlayIntent::Activate(path) => {
                    if path == deck.active_path() {
                        deck.close_overlay();
                    } else if workspace_has_unsaved_surface(&runtime) {
                        deck.set_notice("Save or cancel the current draft before switching.");
                    } else if let Some(prepared) =
                        prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                        && prepare_activation_settings(
                            &mut workspace_config,
                            &mut loader,
                            deck,
                            &root_cwd,
                            &prepared.workspace.path,
                        )
                    {
                        remember_workspace_session_focus(deck, runtime.state());
                        return Ok(WorkspaceStep::Activate(Box::new(prepared)));
                    }
                }
                OverlayIntent::Visit {
                    path,
                    workspace,
                    session,
                } => {
                    if path == deck.active_path() {
                        deck.close_overlay();
                        if workspace == runtime.state().workspace() {
                            let _ = runtime.apply_event(AppEvent::VisitSession(session));
                        }
                    } else if workspace_has_unsaved_surface(&runtime) {
                        deck.set_notice("Save or cancel the current draft before switching.");
                    } else if let Some(prepared) =
                        prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                        && prepare_activation_settings(
                            &mut workspace_config,
                            &mut loader,
                            deck,
                            &root_cwd,
                            &prepared.workspace.path,
                        )
                    {
                        deck.schedule_garden_visit(prepared.workspace.path.clone(), session, None);
                        remember_workspace_session_focus(deck, runtime.state());
                        return Ok(WorkspaceStep::Activate(Box::new(prepared)));
                    }
                }
                OverlayIntent::Add(paths) => {
                    if !paths.is_empty() {
                        if workspace_has_unsaved_surface(&runtime) {
                            deck.set_notice("Save or cancel the current draft before switching.");
                        } else {
                            let current = deck.active_path().to_path_buf();
                            let mut prepared = Vec::with_capacity(paths.len());
                            let mut failed = false;
                            for (index, path) in paths.iter().enumerate() {
                                let label =
                                    format!("Opening workspace {} / {}…", index + 1, paths.len());
                                let Some(snapshot) =
                                    prepare_deck_workspace(term, &mut loader, deck, path, &label)
                                else {
                                    failed = true;
                                    break;
                                };
                                prepared.push(snapshot);
                            }
                            if failed {
                                if let Some(loader) = loader.as_mut() {
                                    let _ = activate_workspace_responsive(
                                        term,
                                        &mut **loader,
                                        &current,
                                        "Restoring current workspace…",
                                    );
                                }
                                prepared.clear();
                            }
                            if let Some(first) = prepared.first().cloned() {
                                if let Some(loader) = loader.as_mut()
                                    && let Err(error) = activate_workspace_responsive(
                                        term,
                                        &mut **loader,
                                        &first.workspace.path,
                                        "Activating workspace…",
                                    )
                                {
                                    deck.set_notice(error.to_string());
                                    drawn_material = None;
                                    frame_source_key = None;
                                    frame_material_key = None;
                                    continue;
                                }
                                if !prepare_batch_settings(
                                    &mut workspace_config,
                                    &mut loader,
                                    deck,
                                    &root_cwd,
                                    &prepared,
                                ) {
                                    drawn_material = None;
                                    frame_source_key = None;
                                    frame_material_key = None;
                                    continue;
                                }
                                // Batch validation leaves the last member selected;
                                // the composition created after this return belongs
                                // to the first newly added tab.
                                if !prepare_activation_settings(
                                    &mut workspace_config,
                                    &mut loader,
                                    deck,
                                    &root_cwd,
                                    &first.workspace.path,
                                ) {
                                    drawn_material = None;
                                    frame_source_key = None;
                                    frame_material_key = None;
                                    continue;
                                }
                                deck.append_snapshots(&prepared);
                                if let Some(loader) = loader.as_mut() {
                                    let _ = (**loader).record_unite(&deck.paths());
                                }
                                remember_workspace_session_focus(deck, runtime.state());
                                return Ok(WorkspaceStep::Activate(Box::new(first)));
                            }
                        }
                    }
                }
                OverlayIntent::Close(path) => {
                    if path == deck.active_path() {
                        let Some(replacement) = deck
                            .replacement_path_after_close(&path)
                            .map(Path::to_path_buf)
                        else {
                            deck.close_path(&path);
                            return Ok(WorkspaceStep::Back);
                        };
                        if workspace_has_unsaved_surface(&runtime) {
                            deck.set_notice("Save or cancel the current draft before closing.");
                        } else if let Some(prepared) = prepare_deck_workspace(
                            term,
                            &mut loader,
                            deck,
                            &replacement,
                            "Opening replacement workspace…",
                        ) && prepare_activation_settings(
                            &mut workspace_config,
                            &mut loader,
                            deck,
                            &root_cwd,
                            &prepared.workspace.path,
                        ) {
                            deck.close_path(&path);
                            remember_workspace_session_focus(deck, runtime.state());
                            return Ok(WorkspaceStep::Activate(Box::new(prepared)));
                        }
                    } else {
                        deck.close_path(&path);
                        if let Some(loader) = loader.as_mut()
                            && let Err(error) = (**loader).record_unite(&deck.paths())
                        {
                            deck.set_notice(error.to_string());
                        }
                    }
                }
            }
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        if garden_pointer_gesture {
            match key {
                Key::Pointer(PointerEvent {
                    kind: PointerKind::Up,
                    ..
                }) => {
                    garden_pointer_gesture = false;
                    continue;
                }
                Key::Pointer(_) => continue,
                _ => garden_pointer_gesture = false,
            }
        }
        // Screen saver admission, before the key is routed anywhere: a wake-up
        // key resets the deadline first, so the frame that wakes the user can
        // never re-open the garden it just closed. A terminal too small to draw
        // a garden emits no idle event at all, leaving its usable Home alone.
        let idle = idle_watch.observe(&key, pointer_clock.elapsed());
        if garden_fits(height, width) {
            let _ = runtime.apply_event(AppEvent::IdleElapsed(idle));
        }
        // The Garden consumes input before the covered pane. List scrolling
        // and clicks need the drawn viewport below; other shell-owned input
        // wakes Home here without reaching its terminal or form.
        if runtime.state().overlay() == Some(Overlay::Garden) {
            if matches!(key, Key::Click { .. }) {
                garden_pointer_gesture = true;
            } else if garden_shell_owned_wake(&key) {
                garden_pointer_gesture = matches!(
                    key,
                    Key::Pointer(PointerEvent {
                        kind: PointerKind::Drag,
                        ..
                    })
                );
                let _ = runtime.apply_event(AppEvent::GardenClick(GardenClick::Dismiss));
                continue;
            }
        }
        // Neither a tick nor a resize refreshes an inventory here any more. Both
        // used to dispatch `RefreshDecisions` + `RefreshSessions`, which ran the
        // daemon round trip on this thread at the 16ms frame cadence; the
        // decision and session lanes are now resident background workers with
        // their own bounded cadence, drained at the head of this loop (#551).
        // A tick and a resize therefore cost exactly one redraw each, and the
        // only wake left is the explicit one a lifecycle action asks for through
        // `ControllerHostAction`.
        let garden_route = route_garden_input(
            &mut ui,
            &mut runtime,
            drawn_material.as_ref(),
            &key,
            &mut garden_pointer_gesture,
        );
        if let Some(GardenInputRoute::Project(visit)) = garden_route {
            let Some(path) = deck
                .path_for_workspace(visit.workspace)
                .map(Path::to_path_buf)
            else {
                deck.open_switcher();
                deck.set_notice("That project is no longer open.");
                garden_pointer_gesture = false;
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
                continue;
            };
            if let Some(prepared) =
                prepare_deck_workspace(term, &mut loader, deck, &path, "Opening workspace…")
                && prepare_activation_settings(
                    &mut workspace_config,
                    &mut loader,
                    deck,
                    &root_cwd,
                    &prepared.workspace.path,
                )
            {
                deck.schedule_garden_visit(
                    prepared.workspace.path.clone(),
                    visit.session,
                    visit.agent,
                );
                remember_workspace_session_focus(deck, runtime.state());
                return Ok(WorkspaceStep::Activate(Box::new(prepared)));
            }
            let notice = deck.notice().map(str::to_owned);
            deck.open_switcher();
            if let Some(notice) = notice {
                deck.set_notice(notice);
            }
            garden_pointer_gesture = false;
            drawn_material = None;
            frame_source_key = None;
            frame_material_key = None;
            continue;
        }
        // The Home header stays above the overlaid Director drawer. Resolve its
        // persistent drawer buttons before the picker gets a chance to consume
        // the click, otherwise an open picker makes the visible buttons inert.
        let pointer_drawer_focus =
            focus_workspace_drawer_from_pointer(&mut runtime, &key, height, width);
        if pointer_drawer_focus.is_some() {
            let focused = runtime.focused_terminal();
            controls.sync_focus(focused.as_ref());
            terminal_rows_len = focused
                .as_ref()
                .and_then(|terminal| ui.terminal_row_extent(terminal, None))
                .map_or(0, |(_, _, total_rows)| total_rows);
            terminal_scroll = controls.scroll();
        }
        let drawer_header_effects = drawn_material.as_ref().and_then(|material| {
            apply_drawer_header_while_director_open(
                &mut runtime,
                &key,
                material.width,
                &material.projection,
            )
        });
        let director_new_effects = drawer_header_effects
            .is_none()
            .then(|| {
                open_director_from_new_button(
                    &mut runtime,
                    &key,
                    height,
                    width,
                    work_run_control.mode(),
                )
            })
            .flatten();
        let director_new_clicked = director_new_effects.is_some();
        if drawer_header_effects.is_some() || director_new_clicked {
            let previous = work_run_control.clone();
            work_run_control.suspend();
            if work_run_control != previous {
                work_run_revision = work_run_revision.wrapping_add(1);
            }
        }
        let work_run_input =
            if garden_route.is_none() && drawer_header_effects.is_none() && !director_new_clicked {
                let previous = work_run_control.clone();
                let input = handle_work_run_control_input_with_ui(
                    Some(&mut ui),
                    &mut runtime,
                    &mut work_run_control,
                    &work_runs,
                    &key,
                );
                if work_run_control != previous {
                    work_run_revision = work_run_revision.wrapping_add(1);
                }
                input
            } else {
                None
            };
        if let Some(WorkRunControlInput {
            outcome: WorkRunControlOutcome::Submit(request),
            ..
        }) = work_run_input.as_ref()
        {
            pending_work_run_control = Some(request.clone());
        }
        let work_run_effects = work_run_input.map(|input| input.effects);
        let input_route = match garden_route {
            Some(GardenInputRoute::Local(effects)) => WorkspaceInputRoute::Garden(effects),
            Some(GardenInputRoute::Agent(effects)) => {
                activate_focused_interrupted_tab(&mut ui, &mut runtime, &mut pending_targets);
                WorkspaceInputRoute::Garden(effects)
            }
            Some(GardenInputRoute::Project(_)) => unreachable!("project visit returned above"),
            None if drawer_header_effects.is_some() => {
                WorkspaceInputRoute::Drawer(drawer_header_effects.expect("matched above"))
            }
            None if director_new_clicked => WorkspaceInputRoute::Drawer(
                director_new_effects.expect("matched Director New button"),
            ),
            None if work_run_effects.is_some() => WorkspaceInputRoute::Drawer(
                work_run_effects.expect("matched Work Run control input"),
            ),
            None => route_workspace_input_before_reducer(
                &mut ui,
                &mut runtime,
                &mut controls,
                term,
                &key,
            ),
        };
        if input_route == WorkspaceInputRoute::Forwarded {
            continue;
        }
        // Live-terminal view controls the reducer does not own (scroll, tab close,
        // pointer drag / copy — design §4.2) are handled before the key reaches
        // the Home reducer.
        if input_route == WorkspaceInputRoute::Unhandled
            && intercept_live_terminal_control(
                &key,
                &mut ui,
                &mut runtime,
                &mut controls,
                term,
                browser.as_mut(),
                &mut pending_targets,
                height,
                width,
                terminal_rows_len,
                terminal_scroll,
            )
        {
            continue;
        }
        if pointer_drawer_focus.is_some()
            && matches!(key, Key::Click { .. })
            && !director_new_clicked
        {
            // Drawer chrome owns its whole rectangle. A click outside the PTY
            // viewport must not activate the Home sidebar hidden underneath.
            continue;
        }
        let daemon_overlay_was_open = runtime.state().overlay() == Some(Overlay::Daemon);
        let effects = if let WorkspaceInputRoute::Drawer(effects)
        | WorkspaceInputRoute::Garden(effects) = input_route
        {
            effects
        } else if let Key::Click { column, row } = key {
            if let Some(route) =
                route_pr_modal_click(runtime.state().overlay(), height, width, column, row)
            {
                if route == PrModalClickRoute::Inside {
                    // The modal owns its whole box; a click there must not
                    // activate a header, pane tab, or sidebar row behind it.
                    Vec::new()
                } else {
                    runtime.apply_event(AppEvent::Key(AppKey::Escape))
                }
            } else {
                // Header rendering and hit-testing share one layout projection, so
                // Notice presence and narrow clipping cannot move an action away
                // from its clickable cells.
                let header_action = drawn_material.as_ref().and_then(|material| {
                    home_header_action_at(width, &material.projection, column, row)
                });
                let pane_tab = runtime
                    .wants_right_pane_tab_click()
                    .then(|| {
                        drawn_material.as_ref().and_then(|material| {
                            right_pane_tab_at(
                                material.height,
                                material.width,
                                &material.projection,
                                column,
                                row,
                            )
                        })
                    })
                    .flatten();
                match (header_action, pane_tab) {
                    (Some(HomeHeaderAction::Director), _) => {
                        runtime.apply_event(AppEvent::Key(AppKey::ToggleDirectorDrawer))
                    }
                    (Some(HomeHeaderAction::RootTerminal), _) => {
                        runtime.apply_event(AppEvent::Key(AppKey::ToggleRootTerminalDrawer))
                    }
                    (Some(HomeHeaderAction::Decisions), _) => {
                        runtime.apply_event(AppEvent::Key(AppKey::OpenDecisions))
                    }
                    (None, Some(index)) => {
                        if select_right_pane_tab(&mut ui, &mut runtime, index) {
                            activate_focused_interrupted_tab(
                                &mut ui,
                                &mut runtime,
                                &mut pending_targets,
                            );
                        }
                        Vec::new()
                    }
                    (None, None) => runtime.apply_event(sidebar_pointer_event(
                        column,
                        row,
                        pointer_clock.elapsed(),
                    )),
                }
            }
        } else {
            runtime.handle_key(key)
        };
        if !daemon_overlay_was_open && runtime.state().overlay() == Some(Overlay::Daemon) {
            ui.refresh_agent_inventory();
        }
        for effect in effects {
            let opens_workspace_config = matches!(
                &effect,
                Effect::WorkspaceCommand {
                    workspace,
                    command: crate::usecase::overview::Command::Config { arguments },
                } if *workspace == workspace_id && arguments.trim().is_empty()
            );
            if opens_workspace_config && let Some(context) = workspace_config.as_mut() {
                // Rebuild after the reducer closed the Overview overlay. The
                // terminal projection is cloned only on this rare Config path;
                // the ordinary frame moved it into `HomeFrameMaterial`.
                let terminal_view = drawn_material
                    .as_ref()
                    .and_then(|material| material.projection.terminal_view().cloned());
                let home = render_controller_frame(
                    height,
                    width,
                    &runtime,
                    &workspace_name,
                    &sessions,
                    metrics_projection.metrics(),
                    metrics_projection.health(),
                    metrics_projection.git_diffs(),
                    terminal_view,
                    ui.creating_session
                        .as_ref()
                        .map(|create| create.name.as_str()),
                );
                // Workspace frames reserve row zero for the project bar. Keep
                // that exact composition behind Config; otherwise the Home
                // projection is drawn one row too high while the modal is open.
                let base = compose_workspace_shell_frame(deck, height, width, &home);
                let branch_catalog = session_branch_catalog(
                    &root_cwd,
                    usagi_core::usecase::settings::read_for_workspace_entry(context.settings)
                        .default_branch
                        .as_deref(),
                );
                run_workspace_config(
                    term,
                    context.settings,
                    context.available_models,
                    &branch_catalog.branches,
                    &base,
                )?;
                // The modal drew over the frame the gate remembers, so the next
                // tick must redraw even if no material changed underneath it.
                drawn_material = None;
                frame_source_key = None;
                frame_material_key = None;
                let effective =
                    usagi_core::usecase::settings::read_for_workspace_entry(context.settings);
                runtime.set_modal_selection_mode(effective.modal_selection_mode);
                runtime.set_pr_auto_open(effective.pr_auto_open);
                // A newly saved Agent default applies to the next `agent`
                // command without reopening the workspace.
                runtime.set_agent_models(context.available_models, effective.default_model);
                runtime.set_work_mode(effective.work_mode);
                let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionBranchCatalog(
                    session_branch_catalog(&root_cwd, effective.default_branch.as_deref()),
                )));
                // Team selection changes the effective role catalog immediately
                // for the next session creation or Agent launch.
                let role_catalog = session_role_catalog(data_home.as_deref(), &root_cwd);
                let _ = runtime.apply_event(AppEvent::Backend(BackendEvent::SessionRoleCatalog(
                    role_catalog,
                )));
                continue;
            }
            // Both stops return from here, which is what performs the teardown:
            // every port, pump, worker, and live-terminal subscription this
            // workspace established is owned by this frame, so returning drops
            // them before the caller can open another workspace (#556).
            match backend.dispatch(effect) {
                BackendFlow::Continue => {}
                BackendFlow::Exit => return Ok(WorkspaceStep::Quit),
                BackendFlow::Leave => return Ok(WorkspaceStep::Back),
            }
        }
    }
}

fn session_role_catalog(data_home: Option<&Path>, workspace_root: &Path) -> SessionRoleCatalog {
    data_home
        .and_then(|data_home| {
            usagi_core::infrastructure::role_catalog::load_effective(data_home, workspace_root).ok()
        })
        .map(|catalog| {
            let roles = catalog
                .roles
                .into_iter()
                .filter(|(_, definition)| {
                    definition
                        .scopes
                        .contains(&usagi_core::domain::role::RoleScope::Session)
                })
                .map(|(id, definition)| RoleChoice {
                    id,
                    summary: definition.summary,
                })
                .collect();
            SessionRoleCatalog {
                roles,
                default: catalog.defaults.session,
            }
        })
        .unwrap_or_default()
}

/// Reads local and remote-tracking branch identities for the create picker.
/// A remote's symbolic `HEAD` is exposed as its `(default)` choice; other
/// symbolic aliases are omitted. A failure shrinks the picker to the daemon's
/// legacy `HEAD` default instead of making the workspace unusable.
fn session_branch_catalog(
    workspace_root: &Path,
    configured_default: Option<&str>,
) -> SessionBranchCatalog {
    let output = usagi_core::infrastructure::git::confined_git_command(workspace_root)
        .args([
            "for-each-ref",
            "--format=%(refname) %(symref)",
            "refs/heads",
            "refs/remotes",
        ])
        .output();
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(_) | Err(_) => return SessionBranchCatalog::default(),
    };
    let branches = parse_session_branch_choices(&String::from_utf8_lossy(&output.stdout));
    let default = configured_default
        .filter(|configured| branches.iter().any(|branch| branch.refname == *configured))
        .map(str::to_owned)
        .or_else(|| {
            branch_default_from_output(
                usagi_core::infrastructure::git::confined_git_command(workspace_root)
                    .args(["symbolic-ref", "--quiet", "HEAD"])
                    .output(),
                &branches,
            )
        });
    SessionBranchCatalog { branches, default }
}

fn branch_default_from_output(
    output: io::Result<std::process::Output>,
    branches: &[BranchChoice],
) -> Option<String> {
    let output = match output {
        Ok(output) if output.status.success() => output,
        Ok(_) | Err(_) => return None,
    };
    let refname = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branches.iter().any(|branch| branch.refname == refname) {
        Some(refname)
    } else {
        None
    }
}

fn parse_session_branch_choices(output: &str) -> Vec<BranchChoice> {
    output
        .lines()
        .filter_map(|line| {
            let (refname, symref) = line.split_once(' ').unwrap_or((line, ""));
            let label = if symref.is_empty() {
                refname
                    .strip_prefix("refs/heads/")
                    .map(|name| format!("local:{name}"))
                    .or_else(|| {
                        refname
                            .strip_prefix("refs/remotes/")
                            .map(|name| format!("remote:{name}"))
                    })
            } else {
                remote_default_branch_label(refname, symref)
            }?;
            Some(BranchChoice {
                label,
                refname: refname.to_owned(),
            })
        })
        .collect()
}

fn remote_default_branch_label(refname: &str, symref: &str) -> Option<String> {
    let name = refname.strip_prefix("refs/remotes/")?;
    let remote = name.strip_suffix("/HEAD")?;
    let target_prefix = format!("refs/remotes/{remote}/");
    (!remote.is_empty() && symref.starts_with(&target_prefix) && symref != refname)
        .then(|| format!("remote:{remote}/(default)"))
}

/// Run the controller-driven workspace runtime, mapping its stop to [`Exit`].
///
/// # Errors
///
/// Returns terminal IO failures from the interactive loop.
#[allow(clippy::too_many_arguments)]
pub fn run_workspace_controller_with_backend(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
) -> io::Result<Exit> {
    let mut deck = WorkspaceDeck::new(&snapshot);
    drive_workspace_controller(
        term,
        snapshot,
        &mut deck,
        &[],
        None,
        backend_factory,
        usagi_core::domain::settings::ModalSelectionMode::Action,
        usagi_core::domain::settings::PrAutoOpen::default(),
        WorkspaceEntryPolicy::default(),
        None,
    )
    .map(WorkspaceStep::exit)
}

/// Run a direct workspace entry with settings already resolved for that
/// workspace identity.
///
/// # Errors
///
/// Returns terminal IO failures from the interactive loop.
pub fn run_workspace_controller_with_backend_and_settings(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
    settings: &usagi_core::domain::settings::Settings,
) -> io::Result<Exit> {
    let mut deck = WorkspaceDeck::new(&snapshot);
    drive_workspace_controller(
        term,
        snapshot,
        &mut deck,
        &[],
        None,
        backend_factory,
        settings.modal_selection_mode,
        settings.pr_auto_open,
        WorkspaceEntryPolicy {
            default_model: settings.default_model,
            default_branch: settings.default_branch.clone(),
            work_mode: settings.work_mode,
            icon_mode: settings.icon_mode,
            ..WorkspaceEntryPolicy::default()
        },
        None,
    )
    .map(WorkspaceStep::exit)
}

/// Run a direct workspace entry with a writable settings port for Overview's
/// workspace-local `config` command.
///
/// # Errors
///
/// Returns workspace binding or terminal IO failures.
pub fn run_workspace_controller_with_backend_and_config(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    backend_factory: &mut dyn ControllerBackendFactory,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    settings.select_workspace(&snapshot.workspace.path)?;
    let effective = usagi_core::usecase::settings::read_for_workspace_entry(settings);
    let mut deck = WorkspaceDeck::new(&snapshot);
    drive_workspace_controller(
        term,
        snapshot,
        &mut deck,
        &[],
        None,
        backend_factory,
        effective.modal_selection_mode,
        effective.pr_auto_open,
        WorkspaceEntryPolicy {
            available_models,
            default_model: effective.default_model,
            default_branch: effective.default_branch.clone(),
            work_mode: effective.work_mode,
            icon_mode: effective.icon_mode,
        },
        Some(WorkspaceConfigContext {
            settings,
            available_models,
        }),
    )
    .map(WorkspaceStep::exit)
}

/// Run a direct workspace entry inside the same process-level deck used by the
/// Welcome/Open graph, so `Ctrl-O +` is available immediately.
///
/// # Errors
///
/// Returns workspace preparation, settings, persistence, or terminal IO
/// failures.
#[allow(clippy::too_many_arguments)]
pub fn run_workspace_deck_with_backend_and_config(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    backend_factory: &mut dyn ControllerBackendFactory,
    settings: &mut dyn SettingsPort,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    enter_workspace(
        term,
        snapshot,
        registry,
        loader,
        settings,
        backend_factory,
        available_models,
    )
    .map(|exit| exit.unwrap_or(Exit::Welcome))
}

struct FixedBackendFactory {
    sessions: Option<Box<dyn SessionCommandPort>>,
    agent: Option<Box<dyn AgentCommandPort>>,
    launch: Option<Box<dyn PaneLaunchCommandPort>>,
    restore: Option<Box<dyn AgentCommandPort>>,
    metrics: Option<Box<dyn MetricsPort>>,
    browser: Option<Box<dyn BrowserOpener>>,
    /// Resident session-inventory lane injected as a fake by the frame-loop
    /// tests; unset means the workspace observes nothing (#551).
    session_refresh: Option<Box<dyn SessionRefreshPort>>,
    /// Decision lane injected as a fake by the frame-loop tests; unset keeps the
    /// unavailable port.
    decisions: Option<Box<dyn BackendDecisionPort>>,
    /// Worktree scan injected as a counting fake by the frame-loop tests; unset
    /// keeps the real `read_dir` (#554).
    session_worktrees: Option<Box<dyn SessionWorktreeScanPort>>,
}

impl ControllerBackendFactory for FixedBackendFactory {
    fn create(
        &mut self,
        _: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition {
        ControllerBackendComposition {
            backend: DaemonBackend::new(
                Box::new(host.clone()),
                Box::new(host),
                Box::new(UnavailableBackendPort),
                Box::new(UnavailableBackendPort),
            )
            .with_decisions(
                self.decisions
                    .take()
                    .unwrap_or_else(|| Box::new(UnavailableBackendPort)),
            )
            .with_overlay(Box::new(UnavailableBackendPort)),
            session_commands: self
                .sessions
                .take()
                .expect("fixed session port is created once"),
            session_refresh: self
                .session_refresh
                .take()
                .unwrap_or_else(|| Box::new(UnavailableSessionRefreshPort)),
            agent_commands: self.agent.take().expect("fixed agent port is created once"),
            pane_launch_commands: self
                .launch
                .take()
                .unwrap_or_else(|| Box::new(UnavailablePaneLaunchPort)),
            restore_commands: self
                .restore
                .take()
                .unwrap_or_else(|| Box::new(UnavailableAgentCommandPort)),
            restore_connection: Box::new(UnavailableRestoreConnectionPort),
            garden_inventory: Box::new(UnavailableGardenInventoryPort),
            work_runs: Box::new(UnavailableWorkRunPort),
            agent_tab_intents: Box::new(UnavailableAgentTabIntentPort),
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            metrics: self
                .metrics
                .take()
                .expect("fixed metrics port is created once"),
            browser: self
                .browser
                .take()
                .expect("fixed browser port is created once"),
            session_worktrees: self
                .session_worktrees
                .take()
                .unwrap_or_else(|| Box::new(UnavailableSessionWorktreeScanPort)),
        }
    }
}

/// Compatibility entry for embedders that still supply individual host ports.
/// Production uses [`run_workspace_controller_with_backend`].
///
/// `agent_port` is the resident terminal stream client and `pane_launch_port` the
/// dedicated launch client: they are separate arguments because they must be
/// separate clients, so a slow launch cannot stop an existing pane's IO.
///
/// # Errors
///
/// Returns terminal IO failures from the interactive workspace loop.
#[allow(clippy::too_many_arguments)]
pub fn run_workspace_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    session_commands: Box<dyn SessionCommandPort>,
    agent_port: Box<dyn AgentCommandPort>,
    pane_launch_port: Box<dyn PaneLaunchCommandPort>,
    _decisions: Box<dyn DecisionCommandPort>,
    _environment: Box<dyn EnvironmentStorePort>,
    _desktop_notifications: Box<dyn DesktopNotificationPort>,
    metrics: Box<dyn MetricsPort>,
    _pr_port: Box<dyn PrSnapshotPort>,
    browser: Box<dyn BrowserOpener>,
) -> io::Result<Exit> {
    let mut factory = FixedBackendFactory {
        sessions: Some(session_commands),
        agent: Some(agent_port),
        launch: Some(pane_launch_port),
        restore: None,
        metrics: Some(metrics),
        browser: Some(browser),
        session_refresh: None,
        decisions: None,
        session_worktrees: None,
    };
    run_workspace_controller_with_backend(term, snapshot, &mut factory)
}

/// Open list 用に、registry の生値と recent projection を結び付ける。
///
/// `Recent::Workspace` は各登録 workspace の集計済み表示値を持つ。互換呼び出しで
/// projection が無いときだけ、生値から 0 件の overview を組み立てる。
fn open_from_registry(workspaces: Vec<Workspace>, recent: &[Recent]) -> Open {
    let open_overviews = recent
        .iter()
        .filter_map(|recent| match recent {
            Recent::Workspace(overview) => Some(overview.clone()),
            Recent::Unite(_) => None,
        })
        .collect::<Vec<_>>();
    if open_overviews.is_empty() && !workspaces.is_empty() {
        Open::new(workspaces)
    } else {
        Open::with_overviews(open_overviews)
    }
}

/// `start` で選んだ画面を起点にした対話 runtime。
///
/// Welcome→Open→Workspace と Welcome→Recent→Workspace は選択 path を同じ [`WorkspaceLoader`]
/// で開き、同じ Workspace runtime を駆動する。Workspace の基底 Switch では Esc は無効で、
/// Closeup や前面 modal を閉じるためだけに使う。workspace では `q` が TUI を閉じ、Ctrl-Q が
/// daemon-owned session を終了してから TUI を閉じる。
///
/// `workspaces` / `recent` / `now` は永続化・実時計を持つ呼び出し側から渡す。
///
/// # Errors
///
/// workspace の読み込み、端末への描画、キー読み取りのいずれかに失敗した場合、そのエラーを返す。
#[allow(clippy::too_many_arguments)] // screen data と注入 port（loader / settings / session port factory）を合成側から受ける入口。
pub fn run_with_settings(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
) -> io::Result<Exit> {
    run_with_settings_inner(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        None,
        None,
        AvailableAgentModels::all(),
    )
}

/// Run the screen graph with daemon Agent and metrics port factories.
///
/// # Errors
///
/// Returns workspace loading or terminal IO failures from the screen graph.
#[allow(clippy::too_many_arguments)]
pub fn run_with_settings_and_agent_and_metrics_port_factory_and_model_availability(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
    agent_commands: &mut dyn AgentCommandPortFactory,
    available_models: AvailableAgentModels,
    metrics: &mut dyn MetricsPortFactory,
) -> io::Result<Exit> {
    run_with_settings_inner(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        session_commands,
        Some(agent_commands),
        Some(metrics),
        available_models,
    )
}

/// Open one workspace snapshot through the controller runtime, supplying
/// fallback ports for the screen-graph entry points that do not inject a daemon
/// Agent / metrics factory (`run_with_settings`).
#[allow(clippy::too_many_arguments)]
fn open_snapshot_via_controller(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    deck: &mut WorkspaceDeck,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<WorkspaceStep> {
    settings.select_workspace(&snapshot.workspace.path)?;
    let effective = usagi_core::usecase::settings::read_for_workspace_entry(settings);
    drive_workspace_controller(
        term,
        snapshot,
        deck,
        registry,
        Some(loader),
        backend_factory,
        effective.modal_selection_mode,
        effective.pr_auto_open,
        WorkspaceEntryPolicy {
            available_models,
            default_model: effective.default_model,
            default_branch: effective.default_branch.clone(),
            work_mode: effective.work_mode,
            icon_mode: effective.icon_mode,
        },
        Some(WorkspaceConfigContext {
            settings,
            available_models,
        }),
    )
}

/// Open one workspace through the controller runtime, then say where the screen
/// graph goes next.
///
/// `Some(exit)` means the TUI itself is finished; `None` means the workspace was
/// left for Welcome and the graph keeps running. Recent, Open, and New all route
/// through this one decision so leaving and quitting cannot diverge between the
/// three entries (#556).
fn enter_workspace(
    term: &mut dyn Terminal,
    snapshot: WorkspaceSnapshot,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<Option<Exit>> {
    let deck = WorkspaceDeck::new(&snapshot);
    enter_workspace_deck(
        term,
        snapshot,
        deck,
        registry,
        loader,
        settings,
        backend_factory,
        available_models,
    )
}

/// Run the process-level deck while keeping exactly one workspace composition
/// resident. A prepared activation returns from the old frame first, so all of
/// its ports are dropped before the next factory call.
#[allow(clippy::too_many_arguments)]
fn enter_workspace_deck(
    term: &mut dyn Terminal,
    mut snapshot: WorkspaceSnapshot,
    mut deck: WorkspaceDeck,
    registry: &[Workspace],
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<Option<Exit>> {
    if deck.slots().len() > 1 {
        let _ = loader.record_unite(&deck.paths());
    }
    loop {
        let step = open_snapshot_via_controller(
            term,
            snapshot,
            &mut deck,
            registry,
            loader,
            settings,
            backend_factory,
            available_models,
        )?;
        match step {
            WorkspaceStep::Quit => return Ok(Some(Exit::Quit)),
            WorkspaceStep::Back => return Ok(None),
            WorkspaceStep::Activate(prepared) => {
                deck.activate_snapshot(&prepared);
                if deck.slots().len() > 1 {
                    let _ = loader.record_unite(&deck.paths());
                }
                snapshot = *prepared;
            }
        }
    }
}

fn prepare_workspace_deck(
    term: &mut dyn Terminal,
    loader: &mut dyn WorkspaceLoader,
    paths: &[PathBuf],
) -> io::Result<(Vec<WorkspaceSnapshot>, WorkspaceSnapshot, WorkspaceDeck)> {
    let Some((first_path, remaining_paths)) = paths.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a project deck needs at least one workspace",
        ));
    };
    let primary = open_workspace_responsive(term, loader, first_path, "Opening workspace 1…")?;
    let mut snapshots = vec![primary.clone()];
    for (index, path) in remaining_paths.iter().enumerate() {
        let label = format!("Opening workspace {} / {}…", index + 2, paths.len());
        snapshots.push(open_workspace_responsive(term, loader, path, &label)?);
    }
    activate_workspace_responsive(
        term,
        loader,
        &primary.workspace.path,
        "Activating workspace…",
    )?;
    let mut deck = WorkspaceDeck::new(&primary);
    deck.append_snapshots(&snapshots);
    Ok((snapshots, primary, deck))
}

struct CompatibilityBackendFactory<'a, 'b, 'c> {
    sessions: &'a mut dyn SessionCommandPortFactory,
    agents: Option<&'b mut dyn AgentCommandPortFactory>,
    metrics: Option<&'c mut dyn MetricsPortFactory>,
}

impl ControllerBackendFactory for CompatibilityBackendFactory<'_, '_, '_> {
    fn create(
        &mut self,
        _: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> ControllerBackendComposition {
        let agent_commands = self.agents.as_deref_mut().map_or_else(
            || -> Box<dyn AgentCommandPort> { Box::new(UnavailableAgentCommandPort) },
            AgentCommandPortFactory::create,
        );
        let metrics = self.metrics.as_deref_mut().map_or_else(
            || -> Box<dyn MetricsPort> { Box::new(NoMetrics) },
            MetricsPortFactory::create,
        );
        let backend = DaemonBackend::new(
            Box::new(host.clone()),
            Box::new(host),
            Box::new(UnavailableBackendPort),
            Box::new(UnavailableBackendPort),
        )
        .with_decisions(Box::new(UnavailableBackendPort))
        .with_overlay(Box::new(UnavailableBackendPort));
        // Each role gets its own client from the factory: the resident stream,
        // the launch client, and the restore client never share an instance.
        let pane_launch_commands = self.agents.as_deref_mut().map_or_else(
            || -> Box<dyn PaneLaunchCommandPort> { Box::new(UnavailablePaneLaunchPort) },
            |factory| {
                Box::new(SerializedPaneLaunchPort::new(factory.create()))
                    as Box<dyn PaneLaunchCommandPort>
            },
        );
        ControllerBackendComposition {
            backend,
            session_commands: self.sessions.create(),
            session_refresh: Box::new(UnavailableSessionRefreshPort),
            agent_commands,
            pane_launch_commands,
            restore_commands: self.agents.as_deref_mut().map_or_else(
                || -> Box<dyn AgentCommandPort> { Box::new(UnavailableAgentCommandPort) },
                AgentCommandPortFactory::create,
            ),
            restore_connection: Box::new(UnavailableRestoreConnectionPort),
            garden_inventory: Box::new(UnavailableGardenInventoryPort),
            work_runs: Box::new(UnavailableWorkRunPort),
            agent_tab_intents: Box::new(UnavailableAgentTabIntentPort),
            external_terminal: Box::new(UnavailableExternalTerminalPort),
            metrics,
            browser: Box::new(UnavailableBrowserOpener),
            session_worktrees: Box::new(UnavailableSessionWorktreeScanPort),
        }
    }
}

// The screen graph is an IO composition boundary.  Its choices are covered by
// the injected loader/port tests; LLVM coverage excludes only this terminal
// loop, consistently with the existing `run_with_settings` entry point.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_with_settings_inner(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    session_commands: &mut dyn SessionCommandPortFactory,
    mut agent_commands: Option<&mut dyn AgentCommandPortFactory>,
    mut metrics: Option<&mut dyn MetricsPortFactory>,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    let mut backend_factory = CompatibilityBackendFactory {
        sessions: session_commands,
        agents: agent_commands.take(),
        metrics: metrics.take(),
    };
    run_screen_graph_with_backend(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        &mut backend_factory,
        available_models,
    )
}

/// Everything an entry screen's frame is a function of.
///
/// The entry screens have no clock and no background lane: each `render_*` is
/// pure in the terminal size and the form on screen, so comparing this value
/// against the last drawn one is an exact redraw test (#554). The `now` the
/// renderers receive is fixed for the whole run and therefore not material.
///
/// Holding the form by value means cloning it once per tick. That is a handful
/// of short strings and paths, kept deliberately in exchange for the full
/// screen build, ANSI parse and cell diff it lets an idle tick skip.
#[derive(Debug, PartialEq, Eq)]
struct EntryFrameMaterial {
    height: usize,
    width: usize,
    form: EntryForm,
    missing_workspace: Option<MissingWorkspacePrompt>,
}

#[derive(Debug, PartialEq, Eq)]
enum EntryForm {
    Welcome(Welcome),
    Open(Open),
    New(New),
    Config(Config),
}

impl EntryFrameMaterial {
    fn new(
        height: usize,
        width: usize,
        screen: Screen,
        welcome: &Welcome,
        open: &Open,
        new_form: &New,
        config_form: &Config,
    ) -> Self {
        let form = match screen {
            Screen::Welcome => EntryForm::Welcome(welcome.clone()),
            Screen::Open => EntryForm::Open(open.clone()),
            Screen::New => EntryForm::New(new_form.clone()),
            Screen::Config => EntryForm::Config(config_form.clone()),
        };
        Self {
            height,
            width,
            form,
            missing_workspace: None,
        }
    }

    fn with_missing_workspace(mut self, prompt: Option<&MissingWorkspacePrompt>) -> Self {
        self.missing_workspace = prompt.cloned();
        self
    }

    fn render(&self, now: DateTime<Utc>) -> Vec<String> {
        let base = match &self.form {
            EntryForm::Welcome(welcome) => welcome::render(self.height, self.width, welcome, now),
            EntryForm::Open(open) => render_open(self.height, self.width, open, now),
            EntryForm::New(form) => new::render(self.height, self.width, form),
            EntryForm::Config(form) => config::render(self.height, self.width, form),
        };
        match self.missing_workspace.as_ref() {
            Some(prompt) => render_missing_workspace_prompt(self.height, self.width, &base, prompt),
            None => base,
        }
    }
}

fn entry_help_context(
    screen: Screen,
    open: &Open,
    config: &Config,
    missing_workspace: bool,
) -> KeyHelpContext {
    if missing_workspace {
        return KeyHelpContext::MissingWorkspace;
    }
    match screen {
        Screen::Welcome => KeyHelpContext::Welcome,
        Screen::Open if open.unregistering_path().is_some() => KeyHelpContext::OpenUnregister,
        Screen::Open if open.cleanup_confirming() => KeyHelpContext::OpenCleanup,
        Screen::Open => KeyHelpContext::Open,
        Screen::New => KeyHelpContext::New,
        Screen::Config => config_help_context(config),
    }
}

fn render_missing_workspace_prompt(
    height: usize,
    width: usize,
    base: &[String],
    prompt: &MissingWorkspacePrompt,
) -> Vec<String> {
    let heading = if prompt.paths.len() == 1 {
        prompt.paths[0].display().to_string()
    } else {
        format!(
            "{} workspace directories no longer exist",
            prompt.paths.len()
        )
    };
    let message = if prompt.paths.len() == 1 {
        "Remove its registry entry? No workspace data is deleted."
    } else {
        "Remove their registry entries? No workspace data is deleted."
    };
    let mut view = ConfirmationView::confirmation("Workspace not found", 64, heading, message);
    view.confirm_label = "remove";
    view.cancel_label = "cancel";
    view.hints = "Enter/y: remove   Esc/n: cancel   ←→/Tab: choose";
    modal::render_confirmation_over(height, width, base, prompt.confirmation, view)
}

/// Production screen graph entry. Every Welcome/Open/Recent/New path creates
/// its workspace runtime through the same backend factory as direct launch.
///
/// # Errors
///
/// Returns workspace loading, settings, or terminal IO failures.
#[allow(clippy::too_many_arguments)]
pub fn run_screen_graph_with_backend(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
) -> io::Result<Exit> {
    run_screen_graph_with_backend_and_notice(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        settings,
        backend_factory,
        available_models,
        None,
    )
}

/// [`run_screen_graph_with_backend`] that opens with `notice` already on the
/// Welcome screen.
///
/// This is what makes an entry that could not open its workspace land *inside*
/// the TUI rather than back at the shell: the composition root turns the failure
/// into the same notice the Recent list would have shown
/// ([`open_failure_notice`]), and the switcher comes up with it. The first frame
/// therefore explains why the requested workspace is not on screen, and the user
/// can pick another one or retry the same one.
///
/// # Errors
///
/// Returns workspace loading, settings, or terminal IO failures.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn run_screen_graph_with_backend_and_notice(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
    settings: &mut dyn SettingsPort,
    backend_factory: &mut dyn ControllerBackendFactory,
    available_models: AvailableAgentModels,
    notice: Option<String>,
) -> io::Result<Exit> {
    let mut registry = workspaces.clone();
    let mut welcome = Welcome::new(recent);
    welcome.set_notice(notice);
    let mut open = open_from_registry(workspaces, welcome.recent());
    let mut new_form = New::default();
    let mut config_form = Config::load_with_available_models(settings, available_models);
    let mut screen = match start {
        Start::Welcome => Screen::Welcome,
        Start::Config => Screen::Config,
    };
    // Material of the frame currently on screen. Every entry screen renders a
    // pure function of its size and its form — there is no clock and no
    // background lane here — so a tick that leaves both unchanged draws
    // nothing (#554).
    let mut drawn_material: Option<EntryFrameMaterial> = None;
    let mut help_context: Option<key_help::State> = None;
    let mut next_create_token = 1_u64;
    let mut pending_create: Option<PendingWorkspaceCreate> = None;
    let mut missing_workspace_prompt: Option<MissingWorkspacePrompt> = None;
    loop {
        let mut created_snapshot = None;
        while let Some(completion) = loader.take_create_completion() {
            let Some(pending) = pending_create.take_if(|pending| {
                pending.token == completion.token && pending.request == completion.request
            }) else {
                continue;
            };
            new_form.finish_create();
            if pending.cancelled {
                let notice = match completion.result {
                    Ok(_) => "creation finished after leaving; workspace was not opened".to_owned(),
                    Err(error) => new_project_notice(&error),
                };
                new_form.set_notice(Some(notice));
                continue;
            }
            match completion.result {
                Ok(snapshot) => {
                    new_form.set_notice(None);
                    created_snapshot = Some(snapshot);
                }
                Err(error) => new_form.set_notice(Some(new_project_notice(&error))),
            }
        }
        if let Some(snapshot) = created_snapshot {
            if !registry_contains_path(&registry, &snapshot.workspace.path) {
                registry.push(snapshot.workspace.clone());
            }
            welcome.record_opened(&snapshot.workspace);
            open.record_opened(&snapshot.workspace);
            if let Some(exit) = enter_workspace(
                term,
                snapshot,
                &registry,
                loader,
                settings,
                backend_factory,
                available_models,
            )? {
                return Ok(exit);
            }
            screen = Screen::Welcome;
            drawn_material = None;
            continue;
        }
        let (height, width) = term.size()?;
        let material = EntryFrameMaterial::new(
            height,
            width,
            screen,
            &welcome,
            &open,
            &new_form,
            &config_form,
        )
        .with_missing_workspace(missing_workspace_prompt.as_ref());
        if drawn_material.as_ref() != Some(&material) {
            let frame = material.render(now);
            let frame = match help_context {
                Some(help) => key_help::render_over(height, width, &frame, help),
                None => frame,
            };
            term.draw(&frame)?;
            drawn_material = Some(material);
        }
        let key = term.read_key()?;
        if let Some(help) = help_context.as_mut() {
            if matches!(key, Key::Help | Key::Escape) {
                help_context = None;
                drawn_material = None;
            } else if scroll_key_help(help, &key, height) {
                drawn_material = None;
            }
            continue;
        }
        if key == Key::Help {
            help_context = Some(key_help::State::new(
                entry_help_context(
                    screen,
                    &open,
                    &config_form,
                    missing_workspace_prompt.is_some(),
                ),
                WorkMode::Classic,
            ));
            drawn_material = None;
            continue;
        }
        let explicit_remove = matches!(&key, Key::Char('y' | 'Y'));
        if let Some(prompt) = missing_workspace_prompt.as_mut() {
            match key {
                Key::Left | Key::Right | Key::Tab => {
                    prompt.confirmation.toggle();
                }
                Key::Char('y' | 'Y') | Key::Enter
                    if explicit_remove || prompt.confirmation.is_confirm_selected() =>
                {
                    let paths = prompt.paths.clone();
                    missing_workspace_prompt = None;
                    let candidates = registry
                        .iter()
                        .filter(|workspace| paths.contains(&workspace.path))
                        .cloned()
                        .collect::<Vec<_>>();
                    let removed = loader.cleanup_missing(&candidates)?;
                    open.remove_paths(&removed);
                    welcome.remove_paths(&removed);
                    remove_registry_paths(&mut registry, &removed);
                    let notice = if removed.is_empty() {
                        "Workspace changed while confirming; nothing was removed. Try again."
                            .to_owned()
                    } else if removed.len() == 1 {
                        "Workspace registration removed.".to_owned()
                    } else {
                        format!("{} workspace registrations removed.", removed.len())
                    };
                    if screen == Screen::Welcome {
                        welcome.set_notice(Some(notice));
                    } else {
                        // Missing-workspace prompts are only created by Welcome
                        // and Open actions, so every non-Welcome prompt belongs
                        // to the Open screen.
                        open.set_notice(Some(notice));
                    }
                }
                Key::Char('n' | 'N') | Key::Escape | Key::Enter => {
                    missing_workspace_prompt = None;
                }
                Key::Quit | Key::CtrlQ => return Ok(Exit::Quit),
                _ => {}
            }
            drawn_material = None;
            continue;
        }
        match screen {
            Screen::Welcome => match step_welcome(&mut welcome, key) {
                WelcomeStep::Stay => {}
                WelcomeStep::Quit => return Ok(Exit::Quit),
                WelcomeStep::OpenList => screen = Screen::Open,
                WelcomeStep::NewForm => {
                    if pending_create
                        .as_ref()
                        .is_some_and(|pending| pending.cancelled)
                    {
                        new_form
                            .set_notice(Some("previous creation is still finishing".to_owned()));
                    }
                    screen = Screen::New;
                }
                WelcomeStep::ConfigScreen => {
                    config_form = Config::load_with_available_models(settings, available_models);
                    screen = Screen::Config;
                }
                WelcomeStep::OpenRecent(index) => {
                    // `Welcome` only creates this action for a visible Recent
                    // number, so the index is fenced by the same model.
                    let recent = &welcome.recent()[index];
                    let paths = recent_paths(recent);
                    if paths.is_empty() {
                        continue;
                    }
                    match loader.missing_paths(&paths) {
                        Ok(missing) if !missing.is_empty() => {
                            missing_workspace_prompt = Some(MissingWorkspacePrompt::new(missing));
                            continue;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            welcome.set_notice(Some(error.to_string()));
                            continue;
                        }
                    }
                    // A workspace this daemon does not serve keeps the switcher on
                    // screen with the reason, so another Recent entry can be tried.
                    let (snapshots, snapshot, deck) =
                        match prepare_workspace_deck(term, loader, &paths) {
                            Ok(prepared) => prepared,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                welcome.set_notice(Some(
                                    "Workspace opening was cancelled.".to_owned(),
                                ));
                                continue;
                            }
                            Err(error) => match open_failure_notice(&error) {
                                Some(notice) => {
                                    welcome.set_notice(Some(notice));
                                    continue;
                                }
                                None => return Err(error),
                            },
                        };
                    welcome.set_notice(None);
                    for snapshot in &snapshots {
                        welcome.record_opened(&snapshot.workspace);
                        open.record_opened(&snapshot.workspace);
                    }
                    if let Some(exit) = enter_workspace_deck(
                        term,
                        snapshot,
                        deck,
                        &registry,
                        loader,
                        settings,
                        backend_factory,
                        available_models,
                    )? {
                        return Ok(exit);
                    }
                    screen = Screen::Welcome;
                }
            },
            Screen::Open => match step_open(&mut open, key) {
                OpenStep::Stay => {}
                OpenStep::Quit => return Ok(Exit::Quit),
                OpenStep::Back => screen = Screen::Welcome,
                OpenStep::Choose(paths) => {
                    match loader.missing_paths(&paths) {
                        Ok(missing) if !missing.is_empty() => {
                            missing_workspace_prompt = Some(MissingWorkspacePrompt::new(missing));
                            continue;
                        }
                        Ok(_) => {}
                        Err(error) => {
                            open.set_notice(Some(error.to_string()));
                            continue;
                        }
                    }
                    // Same contract as Recent: the list stays up with the reason so
                    // the workspace this daemon does serve can be chosen instead.
                    let (snapshots, snapshot, deck) =
                        match prepare_workspace_deck(term, loader, &paths) {
                            Ok(prepared) => prepared,
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                open.set_notice(Some(
                                    "Workspace opening was cancelled.".to_owned(),
                                ));
                                continue;
                            }
                            Err(error) => match open_failure_notice(&error) {
                                Some(notice) => {
                                    open.set_notice(Some(notice));
                                    continue;
                                }
                                None => return Err(error),
                            },
                        };
                    open.set_notice(None);
                    for snapshot in &snapshots {
                        welcome.record_opened(&snapshot.workspace);
                        open.record_opened(&snapshot.workspace);
                    }
                    // Leaving returns to Welcome, not to the list that was used
                    // to get here: all three entries share one way back so the
                    // switcher is always reachable from a workspace.
                    if let Some(exit) = enter_workspace_deck(
                        term,
                        snapshot,
                        deck,
                        &registry,
                        loader,
                        settings,
                        backend_factory,
                        available_models,
                    )? {
                        return Ok(exit);
                    }
                    screen = Screen::Welcome;
                }
                OpenStep::ConfirmCleanup => {
                    let removed = loader.cleanup_missing(&open.workspaces())?;
                    open.remove_paths(&removed);
                    remove_registry_paths(&mut registry, &removed);
                }
                OpenStep::ConfirmUnregister(path) => {
                    let removed = loader.unregister(&[path])?;
                    open.remove_paths(&removed);
                    remove_registry_paths(&mut registry, &removed);
                }
            },
            Screen::New => match step_new(&mut new_form, key) {
                NewStep::Stay => {}
                NewStep::Quit => return Ok(Exit::Quit),
                NewStep::Back => {
                    if let Some(pending) = pending_create.as_mut() {
                        pending.cancelled = true;
                        new_form.finish_create();
                    }
                    screen = Screen::Welcome;
                }
                NewStep::CompleteDirectory(completion) => {
                    let entries = loader
                        .directory_names(completion.parent())
                        .unwrap_or_default();
                    new_form.finish_directory_completion(&completion, entries);
                }
                NewStep::Create(request) => {
                    if pending_create.is_some() {
                        new_form
                            .set_notice(Some("previous creation is still finishing".to_owned()));
                        continue;
                    }
                    let token = WorkspaceCreateToken::new(next_create_token);
                    next_create_token = next_create_token.wrapping_add(1);
                    let effect = WorkspaceCreateEffect {
                        token,
                        request: request.clone(),
                    };
                    match loader.dispatch_create(effect) {
                        Ok(()) => {
                            pending_create = Some(PendingWorkspaceCreate {
                                token,
                                request,
                                cancelled: false,
                            });
                            new_form.begin_create();
                        }
                        Err(error) => {
                            new_form.set_notice(Some(new_project_notice(&error)));
                        }
                    }
                }
            },
            Screen::Config => match step_config(&mut config_form, key, settings) {
                ConfigStep::Stay => {}
                ConfigStep::Quit => return Ok(Exit::Quit),
                ConfigStep::Back => screen = Screen::Welcome,
                ConfigStep::Save => {
                    // The save wave and its `done` hold draw straight to the
                    // terminal, so whatever the gate remembers is no longer on
                    // screen and the next tick must redraw unconditionally.
                    drawn_material = None;
                    if save_config_responsive(term, &mut config_form, settings, None)? {
                        // Hold the `done` confirmation briefly, then return home
                        // with no key press. A failed write skips this and leaves
                        // Config on screen with the error for retry.
                        let (height, width) = term.size()?;
                        term.draw(&config::render(height, width, &config_form))?;
                        term.wait(config::DONE_DISPLAY)?;
                        config_form.reset_save();
                        screen = Screen::Welcome;
                    }
                }
                ConfigStep::SaveEnvironment => {
                    drawn_material = None;
                    let _ = save_environment_responsive(term, &mut config_form, settings);
                }
            },
        }
    }
}

/// Welcome 起動エフェクトを再生し、実際に描いたフレーム数を返す。
///
/// **打鍵で中断できる**。フレーム間の待機は [`Terminal::wait_for_key`] で行い、
/// キーが届いた時点で残りのフレームを捨てて抜ける。中断に使ったキーは
/// **スキップとして消費する**（「何かキーを押すと飛ばせる」の標準的な契約）。
/// これは splash 中に紛れ込んだ端末由来のバイトを次の画面へ流し込まないという
/// 意味でもあり、入力を読まなかった以前の実装よりも取り違えが起きにくい。
/// 起こし待ちの tick と端末リサイズは打鍵ではないため、アニメーションの速度を保つ。
///
/// # Errors
///
/// 端末サイズの取得、描画、フレーム間待機のいずれかに失敗した場合、そのエラーを返す。
pub fn play_startup_splash(term: &mut dyn Terminal) -> io::Result<usize> {
    for frame in 0..splash::FRAMES {
        let (height, width) = term.size()?;
        term.draw(&splash::render(height, width, frame))?;
        match term.wait_for_key(splash::ANIM_TICK)? {
            // 起こし待ちの tick とリサイズは入力ではない。次のフレームは先頭で
            // 端末サイズを読み直すので、リサイズもそのまま追従する。
            None | Some(Key::Other | Key::Resize) => {}
            // それ以外の打鍵は残りのアニメーションをスキップする。
            Some(_) => return Ok(frame + 1),
        }
    }
    Ok(splash::FRAMES)
}

/// 起動スプラッシュの再生権。**1 プロセスで 1 回だけ**再生する。
///
/// workspace を離れて戻ってきた Welcome は「起動」ではないため、2 回目以降の
/// [`Self::play`] は 0 フレームで何も描かない。プロセス内で workspace を切り替える
/// たびに 1.5 秒のアニメーションを見せないための policy であり、合成ルートの都合では
/// なくこの層が持つ（#556）。
#[derive(Debug, Default)]
pub struct StartupSplash {
    played: bool,
}

impl StartupSplash {
    /// まだ再生していない splash を作る。
    #[must_use]
    pub const fn new() -> Self {
        Self { played: false }
    }

    /// 初回だけ splash を再生し、描いたフレーム数を返す。2 回目以降は 0 を返す。
    ///
    /// # Errors
    ///
    /// 再生中の端末操作に失敗した場合、そのエラーを返す。
    pub fn play(&mut self, term: &mut dyn Terminal) -> io::Result<usize> {
        if std::mem::replace(&mut self.played, true) {
            return Ok(0);
        }
        play_startup_splash(term)
    }
}

/// Run the screen graph with transient default settings. Embedders that own a
/// settings backend should call [`run_with_settings`] and inject its port.
///
/// # Errors
///
/// Returns terminal or workspace loading errors from the screen graph.
pub fn run(
    term: &mut dyn Terminal,
    workspaces: Vec<Workspace>,
    recent: Vec<Recent>,
    now: DateTime<Utc>,
    start: Start,
    loader: &mut dyn WorkspaceLoader,
) -> io::Result<Exit> {
    let mut settings = DefaultSettingsPort;
    let mut session_commands = UnavailableSessionCommandPortFactory;
    run_with_settings(
        term,
        workspaces,
        recent,
        now,
        start,
        loader,
        &mut settings,
        &mut session_commands,
    )
}

struct DefaultSettingsPort;

impl SettingsPort for DefaultSettingsPort {
    fn read(
        &mut self,
        _scope: usagi_core::usecase::settings::SettingsScope,
    ) -> io::Result<usagi_core::domain::settings::Settings> {
        Ok(usagi_core::domain::settings::Settings::default())
    }

    fn save(
        &mut self,
        _scope: usagi_core::usecase::settings::SettingsScope,
        _settings: &usagi_core::domain::settings::Settings,
    ) -> io::Result<()> {
        Ok(())
    }
}

/// 選ばれた非対話画面を出力する runner。
///
/// 通常 entry は識別行を、Doctor は注入された診断結果を出力する。出力先とアプリ情報は
/// 呼び出し側から注入するため、実 stdout を直接所有しない。
pub struct BannerScreenRunner<'a, W: Write + ?Sized> {
    out: &'a mut W,
    info: &'a AppInfo,
    doctor_report: Option<&'a crate::usecase::doctor::DoctorReport>,
}

impl<'a, W: Write + ?Sized> BannerScreenRunner<'a, W> {
    /// 注入された出力先とアプリ情報から runner を作る。
    #[must_use]
    pub fn new(out: &'a mut W, info: &'a AppInfo) -> Self {
        Self {
            out,
            info,
            doctor_report: None,
        }
    }

    /// Doctor の診断結果を表示する runner を作る。
    #[must_use]
    pub fn with_doctor_report(
        out: &'a mut W,
        info: &'a AppInfo,
        report: &'a crate::usecase::doctor::DoctorReport,
    ) -> Self {
        Self {
            out,
            info,
            doctor_report: Some(report),
        }
    }

    /// 画面を識別する `label` をアプリ情報とともに一行で書き出す。
    fn write_screen(&mut self, label: &str) -> io::Result<()> {
        writeln!(self.out, "{}: {label}", self.info.describe())
    }
}

impl<W: Write + ?Sized> ScreenRunner for BannerScreenRunner<'_, W> {
    fn welcome(&mut self) -> io::Result<()> {
        self.write_screen("welcome TUI")
    }

    fn workspace(&mut self, path: &Path) -> io::Result<()> {
        self.write_screen(&format!("workspace TUI ({})", path.display()))
    }

    fn config(&mut self) -> io::Result<()> {
        self.write_screen("config TUI")
    }

    fn doctor(&mut self) -> io::Result<()> {
        let report = self.doctor_report.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "doctor report is required")
        })?;
        writeln!(self.out, "{}: doctor", self.info.describe())?;
        for check in &report.checks {
            let status = match check.status {
                crate::usecase::doctor::CheckStatus::Pass => "ok",
                crate::usecase::doctor::CheckStatus::Warning => "warn",
                crate::usecase::doctor::CheckStatus::Fail => "error",
            };
            writeln!(self.out, "[{status}] {}: {}", check.name, check.detail)?;
        }
        writeln!(
            self.out,
            "{}",
            if report.is_healthy() {
                "result: healthy"
            } else {
                "result: problems found"
            }
        )
    }
}

#[cfg(test)]
mod tests;
